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
use crate::runtime_commands::CommandError as RuntimeCommandError;
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
    pub waypipe_version: String,
    pub runtime_binary: String,
    pub container_name: String,
    pub running: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum LaunchError {
    InvalidSession {
        session: String,
        hint: String,
    },
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
    ProcessExited {
        program: String,
        status: std::process::ExitStatus,
        hint: String,
    },
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaunchError::InvalidSession { session, hint } => {
                write!(f, "Invalid container session '{}'. {}", session, hint)
            }
            LaunchError::MissingCommand { command, hint } => {
                write!(f, "Required command '{}' is unavailable. {}", command, hint)
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
                    "Timed out waiting for the Waypipe client socket at '{}'. Check Waypipe stderr, the selected display, and socket directory permissions, then retry.",
                    host_socket
                )
            }
            LaunchError::StartWaypipe { source } => {
                write!(
                    f,
                    "Failed to start waypipe client: {}. Check waypipe_path, executable permissions, and the binary's macOS compatibility; reinstall Waypipe if necessary.",
                    source
                )
            }
            LaunchError::ProcessExited {
                program,
                status,
                hint,
            } => {
                write!(
                    f,
                    "{} exited before startup completed ({}). {}",
                    program, status, hint
                )
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
    let container = require_session_command(
        "container",
        None,
        &child_path,
        "Install Apple Container for this relay, then retry.",
    )
    .map_err(|error| error.to_string())?;

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
                let _ = cleanup_named_runtime_container(
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
    append_guest_command(&mut cmd, image, "perl");
    cmd.arg("-e")
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
    append_guest_command(&mut cmd, image, "perl");
    cmd.arg("-e")
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

#[derive(Default)]
struct RelayStreams {
    sockets: Arc<Mutex<HashMap<u32, Arc<UnixStream>>>>,
}

impl RelayStreams {
    fn get(&self, id: u32) -> Option<Arc<UnixStream>> {
        self.sockets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&id)
            .cloned()
    }

    fn insert(&self, id: u32, socket: Arc<UnixStream>) -> Option<Arc<UnixStream>> {
        self.sockets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, socket)
    }

    fn remove(&self, id: u32) -> Option<Arc<UnixStream>> {
        self.sockets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id)
    }
}

fn retire_relay_stream(
    sockets: &Mutex<HashMap<u32, Arc<UnixStream>>>,
    id: u32,
    socket: &Arc<UnixStream>,
) -> bool {
    let mut sockets = sockets.lock().unwrap_or_else(|error| error.into_inner());
    // An old reader must not remove or send CLOSE for a replacement stream.
    if sockets
        .get(&id)
        .is_some_and(|current| Arc::ptr_eq(current, socket))
    {
        sockets.remove(&id);
        true
    } else {
        false
    }
}

impl Drop for RelayStreams {
    fn drop(&mut self) {
        // Dropping only the map would leave detached reader clones blocked.
        let sockets = std::mem::take(
            &mut *self
                .sockets
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for socket in sockets.values() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
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
    let streams = RelayStreams::default();
    while let Some(frame) = read_relay_frame(reader)? {
        match frame.kind {
            RELAY_FRAME_OPEN => {
                let socket = match connect_unix_socket(local_socket, Duration::from_secs(8)) {
                    Ok(socket) => Arc::new(socket),
                    Err(error) => {
                        eprintln!("cocoa-way container relay stream {}: {}", frame.id, error);
                        let _ = write_relay_frame_locked(&writer, RELAY_FRAME_CLOSE, frame.id, &[]);
                        continue;
                    }
                };
                let mut reader = socket.try_clone().map_err(|error| error.to_string())?;
                let relay_input = Arc::clone(&writer);
                let relay_streams = Arc::clone(&streams.sockets);
                let reader_socket = Arc::clone(&socket);
                let stream_id = frame.id;
                if let Some(previous) = streams.insert(frame.id, socket) {
                    let _ = previous.shutdown(std::net::Shutdown::Both);
                }
                thread::spawn(move || {
                    let mut buffer = [0u8; 64 * 1024];
                    let mut relay_failed = false;
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
                                    relay_failed = true;
                                    break;
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = reader.shutdown(std::net::Shutdown::Both);
                    let retired = retire_relay_stream(&relay_streams, stream_id, &reader_socket);
                    // Release both descriptors before CLOSE can block on the writer.
                    drop(reader);
                    drop(reader_socket);
                    if retired && !relay_failed {
                        let _ = write_relay_frame_locked(
                            &relay_input,
                            RELAY_FRAME_CLOSE,
                            stream_id,
                            &[],
                        );
                    }
                });
            }
            RELAY_FRAME_DATA => {
                if let Some(stream) = streams.get(frame.id) {
                    if (&*stream).write_all(&frame.payload).is_err() {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        streams.remove(frame.id);
                        let _ = write_relay_frame_locked(&writer, RELAY_FRAME_CLOSE, frame.id, &[]);
                    }
                }
            }
            RELAY_FRAME_CLOSE => {
                if let Some(stream) = streams.remove(frame.id) {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
            _ => return Err(format!("unknown container relay frame type {}", frame.kind)),
        }
    }
    Ok(())
}

pub fn apple_publish_socket_supported(container: &Path, child_path: &str) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let Some(supported) = SUPPORTED.get() {
        return *supported;
    }
    let mut command = Command::new(container);
    command.env("PATH", child_path).args(["run", "--help"]);
    let Ok(output) = command_output_with_timeout(command, Duration::from_secs(6)) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let supported = String::from_utf8_lossy(&output.stdout).contains("--publish-socket");
    let _ = SUPPORTED.set(supported);
    supported
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
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("failed to read container relay frame: {}", error)),
        }
    }
    reader
        .read_exact(&mut header[1..])
        .map_err(|error| format!("failed to read container relay frame header: {}", error))?;
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
use IO::Poll qw(POLLIN POLLOUT POLLHUP POLLERR POLLNVAL);
use File::Path qw(make_path);
use POSIX qw(WNOHANG);
use Fcntl qw(F_GETFL F_SETFL O_NONBLOCK);
use Errno qw(EAGAIN EWOULDBLOCK EINTR);
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
$SIG{PIPE} = 'IGNORE';
my $transport = $transport_server->accept() or die "accept transport: $!\n";
binmode $transport;
my $hello = '';
while (length($hello) < 5) {
  my $n = sysread($transport, $hello, 5 - length($hello), length($hello));
  next if !defined $n && $! == EINTR;
  die "transport handshake ended early\n" unless defined $n && $n > 0;
}
die "unsupported transport handshake\n" unless $hello eq "CWV2\x01";
print STDERR "cocoa-way transport v2: host channel connected\n";

# Limits count pending user-space bytes, not kernel buffers. Outer DATA frames
# may be 16 MiB, so consume their payload incrementally rather than allocating
# an entire frame or overflowing a per-peer queue.
my $chunk_size = 65536;
my $peer_limit = 256 * 1024;
my $input_limit = 4 * 1024 * 1024;
# Keep the return-lane backlog small: prequeueing megabytes of bulk would add
# avoidable HOL for later replies even though opposite-direction input runs.
my $output_limit = 256 * 1024;
my $output_frames_limit = 4096;
my $max_peers = 128;
my %peers;
my %peer_ids;
my $next_id = 1;
my @output;
my $output_offset = 0;
my $output_bytes = 0;
my $input_bytes = 0;
my $header = '';
my ($incoming_kind, $incoming_id, $incoming_left);
my $transport_eof = 0;
my $child_exited = 0;
my $exit_code = 0;
my $read_cursor = 0;
my $test_stats = exists $ENV{COCOA_WAY_RELAY_TEST_STATS};
my %stats = map { $_ => 0 } qw(output_high input_high peer_high peers_high output_frames_high peer_chunks_high
  transport_partial peer_partial write_eagain read_eagain eintr signals input_pauses output_pauses closed disconnect_checks);
# Only isolated tests install this handler; production signal dispositions stay unchanged.
$SIG{USR1} = sub { $stats{signals}++ } if exists $ENV{COCOA_WAY_RELAY_TEST_SIGNALS};

sub nonblocking {
  my ($fh) = @_;
  my $flags = fcntl($fh, F_GETFL, 0);
  die "get socket flags: $!\n" unless defined $flags;
  die "set nonblocking: $!\n" unless defined fcntl($fh, F_SETFL, $flags | O_NONBLOCK);
}
nonblocking($transport);
nonblocking($server);

sub send_frame {
  my ($kind, $id, $data) = @_;
  $data = '' unless defined $data;
  my $frame = pack('CNN', $kind, $id, length($data)) . $data;
  die "transport output limit exceeded\n" if $output_bytes + length($frame) > $output_limit;
  die "transport frame limit exceeded\n" if @output >= $output_frames_limit;
  push @output, $frame;
  $output_bytes += length($frame);
  $stats{output_high} = $output_bytes if $test_stats && $output_bytes > $stats{output_high};
  $stats{output_frames_high} = @output if $test_stats && @output > $stats{output_frames_high};
}

sub closing_peer {
  my ($id, $notify, $discard) = @_;
  my $p = $peers{$id} or return;
  $p->{closing} = 1;
  $p->{notify} ||= $notify;
  if ($discard) {
    $input_bytes -= $p->{bytes};
    $p->{bytes} = 0;
    $p->{queue} = [];
    $p->{offset} = 0;
  }
}

sub retire_peers {
  for my $id (sort { $a <=> $b } keys %peers) {
    my $p = $peers{$id};
    next unless $p->{closing} && $p->{bytes} == 0;
    next if $p->{notify} && ($output_bytes + 9 > $output_limit || @output >= $output_frames_limit);
    send_frame(3, $id, '') if $p->{notify};
    delete $peer_ids{fileno($p->{fh})};
    close $p->{fh};
    delete $peers{$id};
    $stats{closed}++ if $test_stats;
  }
}

sub retry_io {
  return 0 if defined $_[0];
  if ($! == EINTR) {
    $stats{eintr}++ if $test_stats;
    return 1;
  }
  if ($! == EAGAIN || $! == EWOULDBLOCK) {
    $stats{$_[1]}++ if $test_stats;
    return 1;
  }
  return 0;
}

sub watch {
  my ($poll, $fh, $flags) = @_;
  $poll->mask($fh => ($poll->mask($fh) || 0) | $flags);
}

RELAY: while (1) {
  if (!$child_exited && waitpid($pid, WNOHANG) == $pid) {
    $child_exited = 1;
    closing_peer($_, 0, 0) for keys %peers;
  }
  retire_peers();
  last RELAY if ($transport_eof || $child_exited) && !$input_bytes && !$output_bytes;

  my $poll = IO::Poll->new();
  # Observe full disconnect even while DATA reads are paused by a slow peer.
  # A write-half EOF still drains completed peer queues; a full HUP cannot.
  watch($poll, $transport, POLLHUP | POLLERR | POLLNVAL);
  watch($poll, $transport, POLLOUT) if $output_bytes;
  if (!$transport_eof && !$child_exited) {
    my $can_read_transport = 1;
    if (defined $incoming_kind && $incoming_kind == 2 && exists $peers{$incoming_id}) {
      my $p = $peers{$incoming_id};
      $can_read_transport = $p->{closing} || ($p->{bytes} < $peer_limit && $input_bytes < $input_limit);
    }
    watch($poll, $transport, POLLIN) if $can_read_transport;
    $stats{input_pauses}++ if $test_stats && !$can_read_transport;
    if (!$can_read_transport && !$output_bytes) {
      # BSD can hide HUP behind unread data. A zero-byte nonblocking send
      # changes no wire bytes but detects a peer that also closed its read end.
      # Check only while input is paused; the idle poll bounds recheck latency.
      $stats{disconnect_checks}++ if $test_stats;
      my $n = send($transport, '', 0);
      if (!defined $n && !retry_io($n, 'write_eagain')) {
        $exit_code = 1;
        last RELAY;
      }
    }
    watch($poll, $server, POLLIN) if keys(%peers) < $max_peers && $output_bytes + 9 <= $output_limit
      && @output < $output_frames_limit;
  }
  for my $p (values %peers) {
    watch($poll, $p->{fh}, POLLOUT) if $p->{bytes};
    if (!$p->{closing} && !$transport_eof && !$child_exited) {
      if ($output_bytes + $chunk_size + 9 <= $output_limit && @output < $output_frames_limit) {
        watch($poll, $p->{fh}, POLLIN);
      } else {
        $stats{output_pauses}++ if $test_stats;
      }
    }
  }
  $! = 0;
  my $ready_count = $poll->poll(0.25);
  if ($ready_count < 0) {
    $stats{eintr}++ if $test_stats && $! == EINTR;
    next RELAY if !$! || $! == EINTR;
    print STDERR "transport select failed: $!\n";
    $exit_code = 1;
    last RELAY;
  }
  if ($poll->events($transport) & (POLLHUP | POLLERR | POLLNVAL)) {
    $exit_code = 1 if $poll->events($transport) & (POLLERR | POLLNVAL);
    last RELAY;
  }

  # Each ready socket gets at most one bounded write; retain every short-write
  # offset. Frames stay FIFO on the transport, and bytes stay FIFO in each peer.
  for my $fh ($poll->handles(POLLOUT)) {
    if ($fh == $transport) {
      my $size = length($output[0]) - $output_offset;
      $size = $chunk_size if $size > $chunk_size;
      my $n = syswrite($transport, $output[0], $size, $output_offset);
      next if retry_io($n, 'write_eagain');
      if (!defined $n || $n == 0) {
        $exit_code = 1;
        last RELAY;
      }
      $stats{transport_partial}++ if $test_stats && $n < $size;
      $output_bytes -= $n;
      $output_offset += $n;
      if ($output_offset == length($output[0])) {
        shift @output;
        $output_offset = 0;
      }
    } else {
      my $id = $peer_ids{fileno($fh)};
      my $p = $peers{$id} or next;
      my $size = length($p->{queue}[0]) - $p->{offset};
      my $n = syswrite($fh, $p->{queue}[0], $size, $p->{offset});
      next if retry_io($n, 'write_eagain');
      if (!defined $n || $n == 0) {
        closing_peer($id, 1, 1);
        next;
      }
      $stats{peer_partial}++ if $test_stats && $n < $size;
      $p->{bytes} -= $n;
      $input_bytes -= $n;
      $p->{offset} += $n;
      if ($p->{offset} == length($p->{queue}[0])) {
        shift @{$p->{queue}};
        $p->{offset} = 0;
      }
    }
  }

  # Rotate peer reads so a busy low-numbered stream cannot monopolize a newly
  # available output slot. This does not reorder bytes inside any stream.
  my @ready = sort {
    (($peer_ids{fileno($a)} // 0) - $read_cursor) % $next_id
      <=> (($peer_ids{fileno($b)} // 0) - $read_cursor) % $next_id
  } $poll->handles(POLLIN | POLLHUP | POLLERR | POLLNVAL);
  for my $fh (@ready) {
    if ($fh == $server) {
      next if keys(%peers) >= $max_peers || $output_bytes + 9 > $output_limit
        || @output >= $output_frames_limit;
      my $peer = $server->accept();
      next unless $peer;
      binmode $peer;
      nonblocking($peer);
      my $id = $next_id++;
      $peers{$id} = { fh => $peer, queue => [], offset => 0, bytes => 0, closing => 0, notify => 0 };
      $peer_ids{fileno($peer)} = $id;
      $stats{peers_high} = keys(%peers) if $test_stats && keys(%peers) > $stats{peers_high};
      send_frame(1, $id, '');
      next;
    }

    if ($fh == $transport) {
      my $size = defined $incoming_kind ? $incoming_left : 9 - length($header);
      $size = $chunk_size if $size > $chunk_size;
      my $p = defined $incoming_kind && $incoming_kind == 2 ? $peers{$incoming_id} : undef;
      $p = undef if $p && $p->{closing};
      if ($p) {
        my $space = $peer_limit - $p->{bytes};
        $space = $input_limit - $input_bytes if $input_limit - $input_bytes < $space;
        $size = $space if $space < $size;
        next unless $size;
      }
      my $data = '';
      my $n = sysread($transport, $data, $size);
      next if retry_io($n, 'read_eagain');
      if (!defined $n || $n == 0) {
        if (!defined $n || length($header) || defined $incoming_kind) {
          print STDERR "truncated or failed transport frame\n";
          $exit_code = 1;
          last RELAY;
        }
        $transport_eof = 1;
        closing_peer($_, 0, 0) for keys %peers;
        next;
      }
      if (!defined $incoming_kind) {
        $header .= $data;
        next unless length($header) == 9;
        ($incoming_kind, $incoming_id, $incoming_left) = unpack('CNN', $header);
        $header = '';
        if ($incoming_left > 16 * 1024 * 1024 || ($incoming_kind != 2 && $incoming_kind != 3)
            || ($incoming_kind == 3 && $incoming_left != 0)) {
          print STDERR "invalid relay frame header\n";
          $exit_code = 1;
          last RELAY;
        }
        closing_peer($incoming_id, 0, 0) if $incoming_kind == 3;
      } else {
        if ($p) {
          if (@{$p->{queue}}) {
            my $space = $chunk_size - length($p->{queue}[-1]);
            $space = length($data) if length($data) < $space;
            $p->{queue}[-1] .= substr($data, 0, $space, '') if $space;
          }
          push @{$p->{queue}}, $data if length($data);
          $p->{bytes} += $n;
          $input_bytes += $n;
          $stats{peer_high} = $p->{bytes} if $test_stats && $p->{bytes} > $stats{peer_high};
          $stats{input_high} = $input_bytes if $test_stats && $input_bytes > $stats{input_high};
          $stats{peer_chunks_high} = @{$p->{queue}} if $test_stats && @{$p->{queue}} > $stats{peer_chunks_high};
        }
        $incoming_left -= $n;
      }
      ($incoming_kind, $incoming_id, $incoming_left) = (undef, undef, undef) unless $incoming_left;
      next;
    }

    my $id = $peer_ids{fileno($fh)};
    my $p = $peers{$id} or next;
    next if $p->{closing} || $output_bytes + $chunk_size + 9 > $output_limit
      || @output >= $output_frames_limit;
    $read_cursor = $id + 1;
    my $data = '';
    my $n = sysread($fh, $data, $chunk_size);
    next if retry_io($n, 'read_eagain');
    if (!defined $n || $n == 0) {
      closing_peer($id, 1, 0);
    } else {
      send_frame(2, $id, $data);
    }
  }
}
for my $id (keys %peers) {
  close $peers{$id}{fh};
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
print STDERR "cocoa-way transport v2 stats: ", join(' ', map { "$_=$stats{$_}" } sort keys %stats), "\n" if $test_stats;
exit $exit_code;
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
    load_sessions_report_from_path(&path)
}

fn load_sessions_report_from_path(path: &Path) -> SessionLoadReport {
    let failure = |action: &str, error: &std::io::Error| SessionLoadReport {
        sessions: vec![],
        error: Some(format!(
            "Failed to {action} {}: {error}. Check the path and file/directory permissions; existing configuration was not replaced.",
            path.display()
        )),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    return failure("create the configuration directory for", &error);
                }
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
            let result = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .and_then(|mut file| file.write_all(example.as_bytes()));
            if let Err(error) = result {
                return failure("create example configuration at", &error);
            }
            log::info!("Created example container-sessions.toml at {:?}", path);
            return SessionLoadReport {
                sessions: vec![],
                error: None,
            };
        }
        Err(error) => return failure("read", &error),
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
                error: Some(format!(
                    "Failed to parse {}: {}. Correct the TOML/session fields and retry; the file was not changed.",
                    path.display(),
                    e
                )),
            }
        }
    }
}

pub fn append_session(session: &ContainerSession) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            "# cocoa-way container sessions\n\
         # Container Mode owns local Linux GUI sessions.\n"
                .into()
        }
        Err(error) => return Err(error),
    };
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
    validate_session(session)?;
    let child_path = build_child_path();
    let waypipe = require_session_command(
        "waypipe",
        session.waypipe_path.as_deref(),
        &child_path,
        "Install Waypipe or correct waypipe_path in container-sessions.toml.",
    )?;
    let runtime_binary_name = runtime_binary_name(&session.runtime);
    let runtime_kind = normalize_container_runtime(&session.runtime);
    let runtime_binary = require_session_command(
        runtime_binary_name,
        None,
        &child_path,
        &format!(
            "Install {} or choose another runtime for this session.",
            runtime_label(&session.runtime)
        ),
    )?;
    let waypipe_version = preflight_waypipe(&waypipe, &child_path)?;
    let warnings = preflight_session(session, runtime_kind, &runtime_binary, &child_path)?;
    let container_name = container_name(session);
    let running =
        runtime_container_is_running(runtime_kind, &runtime_binary, &child_path, &container_name)?;
    Ok(CheckReport {
        runtime: runtime_label(&session.runtime).into(),
        image: session.image.clone(),
        command: session_command(session),
        waypipe: waypipe.display().to_string(),
        waypipe_version,
        runtime_binary: runtime_binary.display().to_string(),
        container_name,
        running,
        warnings,
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

    validate_session(session)?;
    let child_path = build_child_path();
    let waypipe = std::path::PathBuf::from(&check.waypipe);
    require_executable(
        &waypipe,
        "waypipe",
        "Correct waypipe_path or reinstall Waypipe, then validate the profile again.",
    )?;
    let runtime_binary_name = runtime_binary_name(&session.runtime);
    let runtime_kind = normalize_container_runtime(&session.runtime);
    let runtime_binary = require_session_command(
        runtime_binary_name,
        None,
        &child_path,
        &format!(
            "Install {} or choose another runtime for this session.",
            runtime_label(&session.runtime)
        ),
    )?;
    let host_socket = session
        .socket
        .clone()
        .unwrap_or_else(|| default_container_socket(runtime_dir, &session.name));
    let container_socket = session
        .container_socket
        .clone()
        .unwrap_or_else(|| default_container_socket_path(&host_socket, runtime_kind));
    let container_name = container_name(session);
    if runtime_container_is_running(runtime_kind, &runtime_binary, &child_path, &container_name)? {
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
            )
            .map_err(|hint| LaunchError::RuntimeNotReady {
                runtime: runtime_label(&session.runtime).into(),
                hint,
            })?;
        }
        if runtime_container_is_running(
            runtime_kind,
            &runtime_binary,
            &child_path,
            &container_name,
        )? {
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
            cmd.arg("-v").arg(format!(
                "{}:{}",
                socket_parent.display(),
                socket_parent.display()
            ));
            append_guest_command(&mut cmd, &session.image, "sh");
            cmd.args(["-lc", &server_command]);
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
            )
            .map_err(|hint| LaunchError::RuntimeNotReady {
                runtime: runtime_label(&session.runtime).into(),
                hint,
            })?;
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
                "The Apple Container relay was started; guest command readiness is not yet confirmed",
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
            if let Err(error) =
                wait_for_socket(&host_socket, &mut waypipe_child, Duration::from_secs(8))
            {
                let _ = waypipe_child.kill();
                let _ = waypipe_child.wait();
                return Err(error);
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
                "The container process was started; guest command readiness is not yet confirmed",
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
    // A broken image/command must not prevent stopping an existing named session.
    if !matches!(
        session.runtime.trim(),
        "container" | "apple" | "apple-container" | "docker" | "orb" | "orbstack"
    ) {
        return Err(format!(
            "Unknown runtime '{}'; correct this profile's runtime before cleanup.",
            session.runtime
        ));
    }
    let runtime_kind = normalize_container_runtime(&session.runtime);
    if matches!(runtime_kind, ContainerRuntime::OrbStack) {
        return Ok(());
    }

    let child_path = build_child_path();
    let binary_name = runtime_binary_name(&session.runtime);
    let binary = resolve_command_path(binary_name, None, binary_name, &child_path)
        .ok_or_else(|| format!("Missing command `{}`.", binary_name))?;
    let name = container_name(session);
    cleanup_named_runtime_container(runtime_kind, &binary, &child_path, &name)
}

fn cleanup_named_runtime_container(
    runtime_kind: ContainerRuntime,
    binary: &Path,
    child_path: &str,
    name: &str,
) -> Result<(), String> {
    let args: Vec<&str> = match runtime_kind {
        ContainerRuntime::Apple => vec!["delete", "--force", name],
        ContainerRuntime::Docker => vec!["rm", "-f", name],
        ContainerRuntime::OrbStack => return Ok(()),
    };
    let mut command = Command::new(binary);
    command.env("PATH", child_path).args(args);
    match command_output_with_timeout(command, Duration::from_secs(6)) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) if runtime_output_is_not_found(&output.stderr, name) => Ok(()),
        Ok(output) if runtime_output_is_not_found(&output.stdout, name) => Ok(()),
        Ok(output) => {
            let message = format!(
                "Could not remove stale container '{}': {}. Check the runtime service and permissions, remove it manually if safe, then retry.",
                name,
                runtime_failure_detail(&output)
            );
            log::warn!("Failed to cleanup stale container '{}': {}", name, message);
            Err(message)
        }
        Err(RuntimeCommandError::TimedOut) => {
            log::warn!(
                "Timed out cleaning up stale container '{}'; runtime may still be wedged",
                name
            );
            Err(format!(
                "Timed out removing stale container '{}'. Check runtime status and remove it manually if safe, then retry.",
                name
            ))
        }
        Err(RuntimeCommandError::Io(error)) => {
            log::warn!("Failed to cleanup stale container '{}': {}", name, error);
            Err(format!(
                "Failed to remove stale container '{}': {}. Check the runtime executable/service and retry.",
                name, error
            ))
        }
    }
}

fn command_output_with_timeout(
    command: Command,
    timeout: Duration,
) -> Result<Output, RuntimeCommandError> {
    crate::runtime_commands::capture(command, timeout)
}

fn runtime_output_message(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(512).collect())
}

fn runtime_failure_detail(output: &Output) -> String {
    let detail = runtime_output_message(&output.stderr)
        .or_else(|| runtime_output_message(&output.stdout))
        .unwrap_or_else(|| "no diagnostic output".into());
    format!("{}: {}", output.status, detail)
}

fn runtime_output_is_not_found(output: &[u8], name: &str) -> bool {
    let text = String::from_utf8_lossy(output).to_lowercase();
    text.contains("no such container")
        || text.contains("no such object")
        || (text.contains(&name.to_lowercase())
            && (text.contains("not found")
                || text.contains("notfound")
                || text.contains("does not exist")))
}

fn runtime_container_is_running(
    runtime_kind: ContainerRuntime,
    binary: &Path,
    child_path: &str,
    name: &str,
) -> Result<bool, LaunchError> {
    // Even an inventory command can wake OrbStack. Do not probe this runtime here.
    if matches!(runtime_kind, ContainerRuntime::OrbStack) {
        return Ok(false);
    }
    let mut command = Command::new(binary);
    command.env("PATH", child_path);
    let label = match runtime_kind {
        ContainerRuntime::Apple => {
            command.args(["list", "--all"]);
            "Apple Container"
        }
        ContainerRuntime::Docker => {
            command.args(["inspect", "-f", "{{.State.Running}}", name]);
            "Docker"
        }
        ContainerRuntime::OrbStack => unreachable!(),
    };
    let output = command_output_with_timeout(command, Duration::from_secs(6)).map_err(|error| LaunchError::RuntimeNotReady {
        runtime: label.into(),
        hint: format!("Container state check failed: {error}. Check the runtime service and retry; container state is unknown."),
    })?;
    if !output.status.success() {
        if matches!(runtime_kind, ContainerRuntime::Docker)
            && runtime_output_is_not_found(&output.stderr, name)
        {
            return Ok(false);
        }
        return Err(LaunchError::RuntimeNotReady {
            runtime: label.into(),
            hint: format!(
                "Container state check failed ({}). Check the runtime service and retry; container state is unknown.",
                runtime_failure_detail(&output)
            ),
        });
    }
    match runtime_kind {
        ContainerRuntime::Apple => Ok(apple_container_list_has_running(&output.stdout, name)),
        ContainerRuntime::Docker => match String::from_utf8_lossy(&output.stdout).trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(LaunchError::RuntimeNotReady {
                runtime: label.into(),
                hint: "Container state check returned an unrecognized result. Check the Docker CLI/context and retry; container state is unknown.".into(),
            }),
        },
        ContainerRuntime::OrbStack => unreachable!(),
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
    match runtime.trim() {
        "docker" => ContainerRuntime::Docker,
        "orb" | "orbstack" => ContainerRuntime::OrbStack,
        _ => ContainerRuntime::Apple,
    }
}

fn validate_session(session: &ContainerSession) -> Result<(), LaunchError> {
    let invalid = |hint: String| LaunchError::InvalidSession {
        session: session.name.clone(),
        hint,
    };
    if !matches!(
        session.runtime.trim(),
        "container" | "apple" | "apple-container" | "docker" | "orb" | "orbstack"
    ) {
        return Err(invalid(format!(
            "Unknown runtime '{}'. Set runtime to container, docker, or orbstack in container-sessions.toml. SSH connections belong in connections.toml.",
            session.runtime
        )));
    }
    if session.name.trim().is_empty() || session.name.chars().any(char::is_control) {
        return Err(invalid(
            "Set a nonempty name without control characters in container-sessions.toml.".into(),
        ));
    }
    if session.image.trim().is_empty()
        || session
            .image
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
        || session.image.starts_with('-')
    {
        return Err(invalid("Set image to a nonempty local image reference without whitespace or a leading '-' in container-sessions.toml.".into()));
    }
    if session_command(session).trim().is_empty() || session_command(session).contains('\0') {
        return Err(invalid("Set command/app to a nonempty GUI command without NUL bytes in container-sessions.toml.".into()));
    }
    if session.waypipe_threads == Some(0) {
        return Err(invalid(
            "Set waypipe_threads to a positive integer, or remove it to use Waypipe's default."
                .into(),
        ));
    }
    if let Some(socket) = session.socket.as_deref() {
        if !Path::new(socket).is_absolute()
            || socket.contains('\0')
            || socket.len() + WAYPIPE_CLIENT_SUFFIX_RESERVE > UNIX_SOCKET_PATH_BYTES
        {
            return Err(invalid(
                "Set socket to a short absolute host socket path, or remove it to use the default."
                    .into(),
            ));
        }
    }
    if let Some(socket) = session.container_socket.as_deref() {
        if !Path::new(socket).is_absolute()
            || socket.contains('\0')
            || socket.len() > UNIX_SOCKET_PATH_BYTES
        {
            return Err(invalid("Set container_socket to a short absolute guest socket path, or remove it to use the default.".into()));
        }
    }
    validate_display_slot(session)?;
    validate_presentation_command(session)
}

fn require_executable(path: &Path, name: &str, remediation: &str) -> Result<(), LaunchError> {
    use std::os::unix::fs::PermissionsExt;
    let problem = match std::fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => Some("not a regular file".into()),
        Ok(metadata) if metadata.permissions().mode() & 0o111 == 0 => Some("executable permission is missing; restore it only if you trust/own this binary, or reinstall it".into()),
        Ok(_) => None,
        Err(error) => Some(error.to_string()),
    };
    match problem {
        None => Ok(()),
        Some(problem) => Err(LaunchError::MissingCommand {
            command: name.into(),
            hint: format!("Path '{}': {problem}. {remediation}", path.display()),
        }),
    }
}

fn require_session_command(
    name: &str,
    configured: Option<&str>,
    child_path: &str,
    remediation: &str,
) -> Result<std::path::PathBuf, LaunchError> {
    if let Some(path) = resolve_command_path(name, configured, name, child_path) {
        require_executable(&path, name, remediation)?;
        return Ok(path);
    }
    let configured = configured.filter(|path| !path.trim().is_empty());
    let candidate = configured
        .map(|path| {
            let path = path.trim();
            if let Some(rest) = path.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    return std::path::PathBuf::from(home).join(rest);
                }
            }
            std::path::PathBuf::from(path)
        })
        .or_else(|| {
            std::env::split_paths(child_path)
                .map(|dir| dir.join(name))
                .find(|path| path.exists())
        });
    if let Some(path) = candidate {
        require_executable(&path, name, remediation)?;
    }
    Err(LaunchError::MissingCommand {
        command: name.into(),
        hint: format!("Not found in Cocoa-Way's child PATH. {remediation}"),
    })
}

fn preflight_waypipe(binary: &Path, child_path: &str) -> Result<String, LaunchError> {
    let mut command = Command::new(binary);
    command.env("PATH", child_path).arg("--version");
    let failure = |detail: String| LaunchError::MissingCommand {
        command: "waypipe".into(),
        hint: format!(
            "Found at '{}', but its native version check failed: {}. Check this binary's Mac architecture, native libraries, and macOS security approval. Install a working waypipe-darwin build or correct waypipe_path, then retry. Use compatible host and guest Waypipe builds if a protocol/version mismatch is reported.",
            binary.display(),
            detail
        ),
    };
    match command_output_with_timeout(command, Duration::from_secs(3)) {
        Ok(output) if output.status.success() => Ok(runtime_output_message(&output.stdout)
            .or_else(|| runtime_output_message(&output.stderr))
            .unwrap_or_else(|| "unknown (version not reported)".into())),
        Ok(output) => Err(failure(runtime_failure_detail(&output))),
        Err(error) => Err(failure(error.to_string())),
    }
}

fn preflight_session(
    session: &ContainerSession,
    runtime: ContainerRuntime,
    runtime_binary: &Path,
    child_path: &str,
) -> Result<Vec<String>, LaunchError> {
    validate_session(session)?;
    let image_hint = if session.image.starts_with("localhost/") {
        "This localhost/ image is machine-local; it is not bundled with Cocoa-Way or copied to another Mac automatically. Build it on this Mac, import a trusted image archive, or select an installed GUI-ready image in the profile. Do not assume a registry pull can supply this local image."
    } else {
        "Build/tag the image locally, import a trusted image archive, or select an installed GUI-ready image in the profile. Pull it only if the image is available from a registry you trust."
    };
    match runtime {
        ContainerRuntime::Apple => {
            preflight_apple_container(runtime_binary, child_path)?;
            inspect_image(
                runtime_binary,
                child_path,
                &["image", "inspect", &session.image],
                "Apple Container",
                &session.image,
                image_hint,
            )?;
            verify_gui_tools_in_image(
                session,
                runtime_binary,
                child_path,
                runtime,
                "Apple Container",
                &session.image,
            )
        }
        ContainerRuntime::Docker => {
            inspect_image(
                runtime_binary,
                child_path,
                &["image", "inspect", &session.image],
                "Docker",
                &session.image,
                image_hint,
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
        ContainerRuntime::OrbStack => Ok(vec!["OrbStack service, image, guest tools, and running-container state were not checked, to avoid waking OrbStack. Start it explicitly and verify this profile there before launch.".into()]),
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
    const START_HINT: &str = "Use Container > Apple Container > Start System (or `container system start`) and wait until the service reports running. For older CLIs such as 1.0.0, install the latest supported official Apple Container release first.";
    let mut command = Command::new(runtime_binary);
    command
        .env("PATH", child_path)
        .args(["system", "status", "--format", "json"]);
    let output = command_output_with_timeout(command, Duration::from_secs(6)).map_err(|error| {
        LaunchError::RuntimeNotReady {
            runtime: "Apple Container".into(),
            hint: format!("System status check failed: {error}. {START_HINT}"),
        }
    })?;
    if let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        crate::runtime_paths::remember_apple_container_status(&status);
        if output.status.success()
            && status.get("status").and_then(serde_json::Value::as_str) == Some("running")
        {
            return Ok(());
        }
        return Err(LaunchError::RuntimeNotReady {
            runtime: "Apple Container".into(),
            hint: format!(
                "System status was not running ({}). {START_HINT}",
                runtime_failure_detail(&output)
            ),
        });
    }
    // Older CLI versions do not have structured system status.
    let mut command = Command::new(runtime_binary);
    command.env("PATH", child_path).args(["system", "status"]);
    let output = command_output_with_timeout(command, Duration::from_secs(6)).map_err(|error| {
        LaunchError::RuntimeNotReady {
            runtime: "Apple Container".into(),
            hint: format!("System status check failed: {error}. {START_HINT}"),
        }
    })?;
    if !output.status.success() {
        return Err(LaunchError::RuntimeNotReady {
            runtime: "Apple Container".into(),
            hint: format!(
                "System status check failed ({}). {START_HINT}",
                runtime_failure_detail(&output)
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.lines().any(is_apple_container_running_status) {
        return Err(LaunchError::RuntimeNotReady {
            runtime: "Apple Container".into(),
            hint: format!(
                "System status did not report running ({}). {START_HINT}",
                runtime_failure_detail(&output)
            ),
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
    let mut command = Command::new(runtime_binary);
    command.env("PATH", child_path).args(args);
    match command_output_with_timeout(command, Duration::from_secs(6)) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )
            .to_ascii_lowercase();
            let missing = text.contains("no such image")
                || text.contains("image not found")
                || (text.contains(&image.to_ascii_lowercase())
                    && (text.contains("not found") || text.contains("notfound"))
                    && !text.contains("socket"));
            if missing {
                Err(LaunchError::ImageUnavailable {
                    runtime: runtime.into(),
                    image: image.into(),
                    hint: format!("{} {hint}", runtime_failure_detail(&output)),
                })
            } else {
                Err(LaunchError::RuntimeNotReady {
                    runtime: runtime.into(),
                    hint: format!(
                        "Image inspection failed ({}). Check the runtime service/context and permissions; image availability is unknown. Do not rebuild or pull solely because this check failed.",
                        runtime_failure_detail(&output)
                    ),
                })
            }
        }
        Err(error) => Err(LaunchError::RuntimeNotReady {
            runtime: runtime.into(),
            hint: format!("Image inspection failed: {error}. Check the runtime service and retry."),
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
) -> Result<Vec<String>, LaunchError> {
    // Opening the panel can check many profiles at once. Only one probe VM may run at a time.
    static PROBE_LOCK: Mutex<()> = Mutex::new(());
    static PROBE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let deadline = Instant::now() + Duration::from_secs(6);
    let _probe = loop {
        match PROBE_LOCK.try_lock() {
            Ok(lock) => break lock,
            Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(_) => return Err(LaunchError::RuntimeNotReady {
                runtime: runtime_label.into(),
                hint: "Another image dependency check is still running. Wait for it to finish and retry; this image was not checked.".into(),
            }),
        }
    };
    let run_probes = |probes: &[String]| {
        let name = format!(
            "cocoa-way-check-{}-{}",
            std::process::id(),
            PROBE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut cmd = Command::new(runtime_binary);
        cmd.env("PATH", child_path);
        cmd.args(["run", "--rm", "--name", &name]);
        for env in &session.env {
            cmd.arg("--env").arg(env);
        }
        for mount in &session.mounts {
            cmd.arg("--mount").arg(mount);
        }
        for arg in &session.runtime_args {
            cmd.arg(arg);
        }
        if matches!(runtime, ContainerRuntime::Docker) {
            // The preflight must not download images, even if profile arguments request a pull.
            cmd.arg("--pull=never");
        }
        let script = format!(
            "printf '%s\\n' COCOA_WAY_IMAGE_PROBE_STARTED; {}",
            probes.join(" && ")
        );
        append_guest_command(&mut cmd, image, "sh");
        cmd.args(["-lc", &script]);
        let result = command_output_with_timeout(cmd, Duration::from_secs(60));
        if result.is_err() {
            // Killing a CLI does not necessarily stop a VM owned by the runtime daemon.
            let _ = cleanup_named_runtime_container(runtime, runtime_binary, child_path, &name);
        }
        result
    };

    let mut tools = vec!["waypipe".to_string()];
    if matches!(runtime, ContainerRuntime::Apple) {
        tools.push("perl".into());
    }
    if let Some(binary) = session_command_binary(session) {
        tools.push(binary);
    }
    let mut required_probes = tools
        .iter()
        .map(|tool| {
            format!(
                "{{ command -v {} >/dev/null || {{ printf '%s\\n' {} >&2; exit 1; }}; }}",
                shell_single_quote(tool),
                shell_single_quote(&format!("COCOA_WAY_MISSING_TOOL: {tool}"))
            )
        })
        .collect::<Vec<_>>();

    let audio_probe = matches!(runtime, ContainerRuntime::Apple) && session.audio;
    const AUDIO_READY: &str = "COCOA_WAY_AUDIO_TOOLS_READY";
    if audio_probe {
        let tools = [
            "cocoa-way-audio-relay",
            "pipewire",
            "pipewire-pulse",
            "wireplumber",
            "pactl",
            "parec",
        ];
        let probe = tools
            .iter()
            .map(|tool| format!("command -v {tool} >/dev/null"))
            .collect::<Vec<_>>()
            .join(" && ");
        required_probes.push(format!("if {probe}; then printf '%s\\n' {AUDIO_READY}; fi"));
    }

    let mut warnings = Vec::new();
    match run_probes(&required_probes) {
        Ok(output)
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line == "COCOA_WAY_IMAGE_PROBE_STARTED") =>
        {
            if audio_probe
                && !String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line == AUDIO_READY)
            {
                let warning = format!(
                    "Apple Container image '{}' has no complete Cocoa-Way audio stack. Audio is optional; GUI launch can continue with audio unavailable or degraded. Install the Cocoa-Way audio relay/PipeWire tools in the image to enable audio.",
                    image
                );
                log::warn!("{}", warning);
                warnings.push(warning);
            }
        }
        Ok(output) => return Err(image_probe_failure(runtime_label, image, &output)),
        Err(error) => {
            return Err(LaunchError::RuntimeNotReady {
                runtime: runtime_label.into(),
                hint: format!(
                    "Image '{}' dependency check failed: {}. Check runtime status and initial kernel/init-image downloads, then retry before rebuilding the image.",
                    image, error
                ),
            });
        }
    }

    Ok(warnings)
}

fn append_guest_command(command: &mut Command, image: &str, program: &str) {
    command.args(["--entrypoint", program, image]);
}

fn image_probe_failure(runtime: &str, image: &str, output: &Output) -> LaunchError {
    let guest_started = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == "COCOA_WAY_IMAGE_PROBE_STARTED");
    let missing_tool = String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("COCOA_WAY_MISSING_TOOL: "))
        .map(|tool| tool.chars().take(256).collect::<String>());
    if guest_started && missing_tool.is_some() {
        LaunchError::ImageNotGuiReady {
            runtime: runtime.into(),
            image: image.into(),
            hint: format!(
                "Required guest command '{}' was not found ({}). Install it in the image, then rebuild/tag it for Cocoa-Way. Audio tools are optional.",
                missing_tool.unwrap(),
                output.status
            ),
        }
    } else {
        let detail = runtime_failure_detail(output);
        LaunchError::RuntimeNotReady {
            runtime: runtime.into(),
            hint: format!(
                "Image '{image}' did not complete its dependency check: {detail}. Check runtime status, resource limits, and initial kernel/init-image setup before rebuilding the image."
            ),
        }
    }
}

fn runtime_binary_name(runtime: &str) -> &str {
    match runtime.trim() {
        "docker" => "docker",
        "orb" | "orbstack" => "orb",
        _ => "container",
    }
}

fn runtime_label(runtime: &str) -> &str {
    match runtime.trim() {
        "docker" => "Docker",
        "orb" | "orbstack" => "OrbStack",
        _ => "Apple Container",
    }
}

pub fn is_apple_container_session(session: &ContainerSession) -> bool {
    matches!(
        session.runtime.trim(),
        "container" | "apple" | "apple-container"
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

fn wait_for_socket(
    host_socket: &str,
    child: &mut Child,
    timeout: Duration,
) -> Result<(), LaunchError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileTypeExt;

    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|source| LaunchError::StartWaypipe { source })?
        {
            let mut stderr = Vec::new();
            if let Some(pipe) = child.stderr.as_mut() {
                // A descendant can retain stderr after Waypipe exits. Never block on that pipe.
                let fd = pipe.as_raw_fd();
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                if flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0
                {
                    let _ = pipe.take(2048).read_to_end(&mut stderr);
                }
            }
            return Err(LaunchError::ProcessExited {
                program: "Waypipe client".into(),
                status,
                hint: format!(
                    "{} Check Waypipe compatibility, the selected Wayland display, and socket directory permissions, then retry.",
                    runtime_output_message(&stderr)
                        .unwrap_or_else(|| "No stderr was captured.".into())
                ),
            });
        }
        if std::fs::metadata(host_socket).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(LaunchError::SocketTimeout {
                host_socket: host_socket.into(),
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
            shell_single_quote(include_str!(
                "../examples/container-images/cocoa-way-clipboard-relay"
            )),
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

    fn mock_runtime(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("container");
        std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        (directory, binary)
    }

    fn diagnostic_session(runtime: &str) -> ContainerSession {
        toml::from_str(&format!(
            "name = 'Diagnostics'\nimage = 'test:latest'\ncommand = 'foot'\nruntime = '{runtime}'"
        ))
        .unwrap()
    }

    #[test]
    fn missing_config_creates_an_optional_empty_example() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config/sessions.toml");
        let report = load_sessions_report_from_path(&path);
        assert!(report.error.is_none());
        assert!(report.sessions.is_empty());
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("# [[session]]")
        );
    }

    #[test]
    fn unreadable_or_invalid_config_is_reported_and_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.toml");
        for content in [b"\xff".as_slice(), b"[[session]]\nname = [".as_slice()] {
            std::fs::write(&path, content).unwrap();
            let report = load_sessions_report_from_path(&path);
            let error = report.error.unwrap();
            assert!(error.contains(&path.display().to_string()));
            assert!(error.contains("permissions") || error.contains("Correct the TOML"));
            assert_eq!(std::fs::read(&path).unwrap(), content);
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(load_sessions_report_from_path(&path).error.is_some());
        assert!(path.is_dir());
    }

    #[test]
    fn config_creation_failures_are_not_reported_as_success() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("not-a-directory");
        std::fs::write(&parent, "keep").unwrap();
        let report = load_sessions_report_from_path(&parent.join("sessions.toml"));
        assert!(report.error.is_some());
        assert_eq!(std::fs::read_to_string(parent).unwrap(), "keep");
    }

    #[test]
    fn invalid_profiles_fail_before_resolving_or_probing_runtimes() {
        for runtime in ["dokcer", "ssh", ""] {
            let error = check_session(&diagnostic_session(runtime)).unwrap_err();
            assert!(matches!(error, LaunchError::InvalidSession { .. }));
            assert!(error.to_string().contains("Unknown runtime"));
        }
        for image in ["", "bad image", "--help"] {
            let mut session = diagnostic_session("docker");
            session.image = image.into();
            assert!(matches!(
                check_session(&session),
                Err(LaunchError::InvalidSession { .. })
            ));
        }
        let mut session = diagnostic_session("docker");
        session.command = Some(" ".into());
        assert!(validate_session(&session).is_err());
        session.command = Some("foot".into());
        session.socket = Some("relative.sock".into());
        assert!(validate_session(&session).is_err());
        session.socket = None;
        session.waypipe_threads = Some(0);
        assert!(validate_session(&session).is_err());
        for runtime in [
            "container",
            "apple",
            "apple-container",
            "docker",
            "orb",
            "orbstack",
        ] {
            assert!(validate_session(&diagnostic_session(runtime)).is_ok());
        }
    }

    #[test]
    fn executable_preflight_identifies_configured_path_and_permission_errors() {
        use std::os::unix::fs::PermissionsExt;
        let (directory, binary) = mock_runtime("exit 0");
        let path = binary.to_str().unwrap();
        assert!(require_session_command("waypipe", Some(path), "", "Reinstall Waypipe.").is_ok());
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = require_session_command("waypipe", Some(path), "", "Reinstall Waypipe.")
            .unwrap_err()
            .to_string();
        assert!(error.contains(path));
        assert!(error.contains("executable permission"));
        assert!(error.contains("Reinstall Waypipe"));
        let missing = directory.path().join("missing");
        assert!(
            require_session_command(
                "waypipe",
                Some(missing.to_str().unwrap()),
                "",
                "Install it."
            )
            .unwrap_err()
            .to_string()
            .contains("missing")
        );
        assert!(
            require_executable(directory.path(), "waypipe", "Reinstall it.")
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
    }

    #[test]
    fn native_waypipe_check_preserves_version_and_reports_loader_failures() {
        for version in ["0.9.2-darwin", "0.10.6"] {
            let (_directory, binary) = mock_runtime(&format!("printf 'waypipe {version}\\n'"));
            assert_eq!(
                preflight_waypipe(&binary, "/usr/bin:/bin").unwrap(),
                format!("waypipe {version}")
            );
        }
        let (_directory, binary) =
            mock_runtime("printf 'dyld: Library not loaded: libexample.dylib\\n' >&2; exit 127");
        let error = preflight_waypipe(&binary, "/usr/bin:/bin")
            .unwrap_err()
            .to_string();
        assert!(error.contains("libexample.dylib"));
        assert!(error.contains("127"));
        assert!(error.contains("Mac architecture"));
        assert!(error.contains("waypipe-darwin"));
        assert!(error.contains(binary.to_str().unwrap()));
        assert!(
            preflight_waypipe(
                Path::new("/nonexistent/cocoa-way-test-waypipe"),
                "/usr/bin:/bin"
            )
            .is_err()
        );
    }

    #[test]
    fn orbstack_checks_do_not_execute_any_runtime_command() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("woken");
        let (_binary_directory, binary) = mock_runtime(&format!(
            "touch {}\nexit 1",
            shell_single_quote(marker.to_str().unwrap())
        ));
        let session = diagnostic_session("orbstack");
        let warnings = preflight_session(
            &session,
            ContainerRuntime::OrbStack,
            &binary,
            "/usr/bin:/bin",
        )
        .unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not checked"))
        );
        assert!(
            !runtime_container_is_running(
                ContainerRuntime::OrbStack,
                &binary,
                "/usr/bin:/bin",
                "test"
            )
            .unwrap()
        );
        assert!(!marker.exists());
    }

    #[test]
    fn docker_preflight_requires_no_apple_runtime_or_relay_tools_and_never_pulls() {
        let directory = tempfile::tempdir().unwrap();
        let calls = directory.path().join("calls");
        let (_binary_directory, binary) = mock_runtime(&format!(
            "printf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = run ]; then printf 'COCOA_WAY_IMAGE_PROBE_STARTED\\n'; fi",
            shell_single_quote(calls.to_str().unwrap())
        ));
        let mut session = diagnostic_session("docker");
        session.runtime_args.push("--pull=always".into());
        assert!(
            preflight_session(&session, ContainerRuntime::Docker, &binary, "/usr/bin:/bin")
                .unwrap()
                .is_empty()
        );
        let calls = std::fs::read_to_string(calls).unwrap();
        assert!(calls.contains("image inspect test:latest"));
        assert!(calls.contains("--pull=never"));
        assert!(calls.find("--pull=always").unwrap() < calls.find("--pull=never").unwrap());
        assert!(!calls.contains("system status"));
        assert!(!calls.contains("command -v 'perl'"));
    }

    #[test]
    fn image_inspection_preserves_service_errors_and_only_labels_known_missing_images() {
        for (message, missing) in [
            ("Cannot connect to the Docker daemon", false),
            ("permission denied", false),
            ("No such image: test:latest", true),
        ] {
            let (_directory, binary) = mock_runtime(&format!(
                "printf '%s\\n' {} >&2; exit 7",
                shell_single_quote(message)
            ));
            let error = inspect_image(
                &binary,
                "/usr/bin:/bin",
                &["image", "inspect", "test:latest"],
                "Docker",
                "test:latest",
                "Build/tag the image.",
            )
            .unwrap_err();
            assert_eq!(
                matches!(error, LaunchError::ImageUnavailable { .. }),
                missing
            );
            assert!(error.to_string().contains(message));
            assert!(error.to_string().contains("7"));
        }
    }

    #[test]
    fn missing_machine_local_image_explains_migration_and_local_remedies() {
        let (_directory, binary) = mock_runtime(
            "if [ \"$1\" = system ]; then printf '%s\\n' '{\"status\":\"running\"}'; exit 0; fi\nprintf 'image localhost/cocoa-way-niri:latest not found\\n' >&2; exit 1",
        );
        let mut session = diagnostic_session("container");
        session.image = "localhost/cocoa-way-niri:latest".into();
        let error = preflight_session(&session, ContainerRuntime::Apple, &binary, "/usr/bin:/bin")
            .unwrap_err();
        assert!(matches!(error, LaunchError::ImageUnavailable { .. }));
        let detail = error.to_string();
        assert!(detail.contains("not bundled"));
        assert!(detail.contains("another Mac"));
        assert!(detail.contains("Build it on this Mac"));
        assert!(detail.contains("import"));
        assert!(detail.contains("installed GUI-ready image"));
        assert!(!detail.contains("image pull"));
    }

    #[test]
    fn stopped_apple_service_is_reported_before_local_image_availability() {
        let (_directory, binary) = mock_runtime(
            "if [ \"$1\" = system ]; then printf '%s\\n' '{\"status\":\"stopped\"}'; exit 0; fi\nprintf 'image localhost/cocoa-way-niri:latest not found\\n' >&2; exit 1",
        );
        let mut session = diagnostic_session("container");
        session.image = "localhost/cocoa-way-niri:latest".into();
        let error = preflight_session(&session, ContainerRuntime::Apple, &binary, "/usr/bin:/bin")
            .unwrap_err();
        assert!(matches!(error, LaunchError::RuntimeNotReady { .. }));
        let detail = error.to_string();
        assert!(detail.contains("not running"));
        assert!(detail.contains("container system start"));
        assert!(!detail.contains("image pull"));
        assert!(!detail.contains("not available locally"));
    }

    #[test]
    fn failed_container_inventory_does_not_claim_the_container_is_stopped() {
        let (_directory, binary) = mock_runtime("printf 'permission denied\\n' >&2; exit 1");
        for runtime in [ContainerRuntime::Apple, ContainerRuntime::Docker] {
            assert!(matches!(
                runtime_container_is_running(runtime, &binary, "/usr/bin:/bin", "test"),
                Err(LaunchError::RuntimeNotReady { .. })
            ));
        }
        let (_directory, binary) =
            mock_runtime("printf 'Error: No such object: test\\n' >&2; exit 1");
        assert!(
            !runtime_container_is_running(
                ContainerRuntime::Docker,
                &binary,
                "/usr/bin:/bin",
                "test"
            )
            .unwrap()
        );
        let (_directory, binary) = mock_runtime("printf 'unexpected-result\\n'");
        assert!(
            runtime_container_is_running(
                ContainerRuntime::Docker,
                &binary,
                "/usr/bin:/bin",
                "test"
            )
            .is_err()
        );
    }

    #[test]
    fn cleanup_failures_preserve_the_actual_error_instead_of_returning_success() {
        let (_directory, binary) =
            mock_runtime("printf 'runtime service not found\\n' >&2; exit 7");
        let error = cleanup_named_runtime_container(
            ContainerRuntime::Apple,
            &binary,
            "/usr/bin:/bin",
            "test",
        )
        .unwrap_err();
        assert!(error.contains("runtime service not found"));
        assert!(error.contains("7"));
        let (_directory, binary) = mock_runtime("printf 'container test not found\\n' >&2; exit 1");
        assert!(
            cleanup_named_runtime_container(
                ContainerRuntime::Apple,
                &binary,
                "/usr/bin:/bin",
                "test"
            )
            .is_ok()
        );
        assert!(runtime_output_is_not_found(
            b"Error: notFound: container test does not exist",
            "test"
        ));
        let mut session = diagnostic_session("orbstack");
        session.image.clear();
        assert!(cleanup_named_session(&session).is_ok());
        session.runtime = "typo".into();
        assert!(cleanup_named_session(&session).is_err());
    }

    #[test]
    fn guest_probe_script_names_the_required_missing_gui_command() {
        let (directory, binary) =
            mock_runtime("for arg do script=\"$arg\"; done\n/bin/sh -c \"$script\"");
        for tool in ["waypipe", "perl"] {
            std::os::unix::fs::symlink("/usr/bin/true", directory.path().join(tool)).unwrap();
        }
        let mut session = diagnostic_session("container");
        session.audio = false;
        session.command = Some("cocoa-way-test-missing-gui-20261015".into());
        let child_path = format!("{}:/usr/bin:/bin", directory.path().display());
        let error = verify_gui_tools_in_image(
            &session,
            &binary,
            &child_path,
            ContainerRuntime::Apple,
            "Apple Container",
            &session.image,
        )
        .unwrap_err();
        assert!(matches!(error, LaunchError::ImageNotGuiReady { .. }));
        assert!(
            error
                .to_string()
                .contains("cocoa-way-test-missing-gui-20261015")
        );
    }

    #[test]
    fn audio_tools_are_optional_and_absence_is_visible_in_check_warnings() {
        for (audio_ready, expected_warnings) in [(false, 1), (true, 0)] {
            let (_directory, binary) = mock_runtime(&format!(
                "printf 'COCOA_WAY_IMAGE_PROBE_STARTED\\n{}'",
                if audio_ready {
                    "COCOA_WAY_AUDIO_TOOLS_READY\\n"
                } else {
                    ""
                }
            ));
            let session = diagnostic_session("container");
            let warnings = verify_gui_tools_in_image(
                &session,
                &binary,
                "/usr/bin:/bin",
                ContainerRuntime::Apple,
                "Apple Container",
                &session.image,
            )
            .unwrap();
            assert_eq!(warnings.len(), expected_warnings);
        }
    }

    #[test]
    fn successful_runtime_exit_without_guest_probe_is_not_validation_success() {
        let (_directory, binary) = mock_runtime("exit 0");
        let session = diagnostic_session("docker");
        assert!(matches!(
            verify_gui_tools_in_image(
                &session,
                &binary,
                "/usr/bin:/bin",
                ContainerRuntime::Docker,
                "Docker",
                &session.image
            ),
            Err(LaunchError::RuntimeNotReady { .. })
        ));
    }

    #[test]
    fn socket_wait_reports_early_waypipe_exit_with_stderr() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf 'incompatible Waypipe option\\n' >&2; exit 64"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let error = wait_for_socket(
            "/nonexistent/cocoa-way-test.sock",
            &mut child,
            Duration::from_secs(8),
        )
        .unwrap_err();
        assert!(matches!(error, LaunchError::ProcessExited { .. }));
        assert!(error.to_string().contains("incompatible Waypipe option"));
        assert!(error.to_string().contains("64"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn socket_wait_requires_an_actual_socket_not_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("socket");
        std::fs::write(&path, "not a socket").unwrap();
        let mut child = Command::new("/bin/sleep").arg("5").spawn().unwrap();
        let result = wait_for_socket(
            path.to_str().unwrap(),
            &mut child,
            Duration::from_millis(100),
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(matches!(result, Err(LaunchError::SocketTimeout { .. })));
    }

    #[test]
    fn apple_preflight_accepts_structured_status() {
        let (_directory, binary) = mock_runtime("printf '%s\\n' '{\"status\":\"running\"}'");
        assert!(preflight_apple_container(&binary, "/usr/bin:/bin").is_ok());
    }

    #[test]
    fn apple_preflight_does_not_override_structured_stopped_status_with_legacy_text() {
        let (_directory, binary) = mock_runtime(
            "if [ \"$3\" = --format ]; then printf '%s\\n' '{\"status\":\"unregistered\"}'; exit 1; fi\nprintf 'status running\\n'",
        );
        assert!(matches!(
            preflight_apple_container(&binary, "/usr/bin:/bin"),
            Err(LaunchError::RuntimeNotReady { .. })
        ));
    }

    #[test]
    fn apple_preflight_supports_cli_without_json_status() {
        let (_directory, binary) = mock_runtime(
            "if [ \"$3\" = --format ]; then printf 'Unknown option --format\\n' >&2; exit 64; fi\nprintf 'status running\\n'",
        );
        assert!(preflight_apple_container(&binary, "/usr/bin:/bin").is_ok());
    }

    #[test]
    fn unregistered_apple_service_preserves_remote_error_and_start_upgrade_guidance() {
        let (_directory, binary) = mock_runtime(
            "if [ \"$3\" = --format ]; then printf 'Unknown option --format\\n' >&2; exit 64; fi\nprintf 'apiserver is not running and not registered with launchd\\n' >&2; exit 1",
        );
        let error = preflight_apple_container(&binary, "/usr/bin:/bin").unwrap_err();
        assert!(matches!(error, LaunchError::RuntimeNotReady { .. }));
        let detail = error.to_string();
        assert!(detail.contains("apiserver is not running and not registered with launchd"));
        assert!(detail.contains("Start System"));
        assert!(detail.contains("install the latest supported official Apple Container release"));
        assert!(detail.contains("1.0.0"));
        assert!(!detail.contains("image pull"));
    }

    #[test]
    fn failed_image_inspection_command_is_a_runtime_error() {
        assert!(matches!(
            inspect_image(
                Path::new("/nonexistent/cocoa-way-test-runtime"),
                "/usr/bin:/bin",
                &["image", "inspect", "test:latest"],
                "Apple Container",
                "test:latest",
                "Pull it first.",
            ),
            Err(LaunchError::RuntimeNotReady { .. })
        ));
    }

    #[test]
    fn simultaneous_image_checks_do_not_run_multiple_probe_vms() {
        let directory = tempfile::tempdir().unwrap();
        let lock = shell_single_quote(&directory.path().join("probe.lock").display().to_string());
        let (_binary_directory, binary) = mock_runtime(&format!(
            "mkdir {lock} || exit 1\nsleep 0.1\nrmdir {lock}\nprintf 'COCOA_WAY_IMAGE_PROBE_STARTED\\n'"
        ));
        let session: ContainerSession = toml::from_str(
            "name = 'Probe'\nimage = 'test:latest'\ncommand = 'foot'\naudio = false",
        )
        .unwrap();
        thread::scope(|scope| {
            let handles = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        verify_gui_tools_in_image(
                            &session,
                            &binary,
                            "/usr/bin:/bin",
                            ContainerRuntime::Apple,
                            "Apple Container",
                            &session.image,
                        )
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        });
    }

    #[test]
    fn dependency_probe_distinguishes_bootstrap_errors_from_missing_tools() {
        use std::os::unix::process::ExitStatusExt;
        let mut output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"Error: init-image download failed".to_vec(),
        };
        assert!(matches!(
            image_probe_failure("Apple Container", "gui:latest", &output),
            LaunchError::RuntimeNotReady { .. }
        ));
        output.stdout = b"COCOA_WAY_IMAGE_PROBE_STARTED\n".to_vec();
        assert!(matches!(
            image_probe_failure("Apple Container", "gui:latest", &output),
            LaunchError::RuntimeNotReady { .. }
        ));
        output.stderr = b"COCOA_WAY_MISSING_TOOL: perl\n".to_vec();
        assert!(matches!(
            image_probe_failure("Apple Container", "gui:latest", &output),
            LaunchError::ImageNotGuiReady { .. }
        ));
        assert!(
            image_probe_failure("Apple Container", "gui:latest", &output)
                .to_string()
                .contains("perl")
        );
    }

    #[test]
    fn guest_command_overrides_an_images_entrypoint() {
        let mut command = Command::new("container");
        command.arg("run");
        append_guest_command(&mut command, "custom/gui:latest", "perl");
        command.args(["-e", "relay"]);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "run",
                "--entrypoint",
                "perl",
                "custom/gui:latest",
                "-e",
                "relay"
            ]
        );
    }

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
    fn relay_frame_distinguishes_clean_eof_from_truncation() {
        assert!(read_relay_frame(&mut &b""[..]).unwrap().is_none());
        let mut encoded = Vec::new();
        write_relay_frame(&mut encoded, RELAY_FRAME_DATA, 7, b"payload").unwrap();
        for length in 1..encoded.len() {
            assert!(read_relay_frame(&mut &encoded[..length]).is_err());
        }
    }

    #[test]
    fn relay_frame_retries_interrupted_first_byte() {
        struct InterruptedOnce {
            interrupted: bool,
            data: std::io::Cursor<Vec<u8>>,
        }
        impl Read for InterruptedOnce {
            fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.data.read(data)
            }
        }
        let mut encoded = Vec::new();
        write_relay_frame(&mut encoded, RELAY_FRAME_CLOSE, 7, &[]).unwrap();
        let mut reader = InterruptedOnce {
            interrupted: false,
            data: std::io::Cursor::new(encoded),
        };
        let frame = read_relay_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.kind, RELAY_FRAME_CLOSE);
        assert_eq!(frame.id, 7);
    }

    #[test]
    fn relay_stream_teardown_shuts_down_surviving_reader_clone() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        let mut reader = socket.try_clone().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let streams = RelayStreams::default();
        streams.insert(7, Arc::new(socket));
        drop(streams);
        let mut byte = [0u8; 1];
        assert_eq!(reader.read(&mut byte).unwrap(), 0);
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn relay_stream_host_eof_churn_reclaims_owned_sockets_without_guest_close() {
        let streams = RelayStreams::default();
        for id in 1..=256 {
            let (socket, peer) = UnixStream::pair().unwrap();
            let socket = Arc::new(socket);
            let weak = Arc::downgrade(&socket);
            let mut reader = socket.try_clone().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            streams.insert(id, Arc::clone(&socket));
            let reader_streams = Arc::clone(&streams.sockets);
            drop(peer);
            let retired = thread::spawn(move || {
                assert_eq!(reader.read(&mut [0u8; 1]).unwrap(), 0);
                let _ = reader.shutdown(std::net::Shutdown::Both);
                let retired = retire_relay_stream(&reader_streams, id, &socket);
                drop(reader);
                drop(socket);
                retired
            })
            .join()
            .unwrap();
            assert!(retired);
            assert!(streams.get(id).is_none());
            assert!(weak.upgrade().is_none());
            assert!(streams.sockets.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn relay_stream_retirement_preserves_replacement_with_same_id() {
        let streams = RelayStreams::default();
        let (previous, _previous_peer) = UnixStream::pair().unwrap();
        let previous = Arc::new(previous);
        streams.insert(7, Arc::clone(&previous));
        let (replacement, _replacement_peer) = UnixStream::pair().unwrap();
        let replacement = Arc::new(replacement);
        assert!(streams.insert(7, Arc::clone(&replacement)).is_some());
        assert!(!retire_relay_stream(&streams.sockets, 7, &previous));
        assert!(Arc::ptr_eq(&streams.get(7).unwrap(), &replacement));
        assert!(retire_relay_stream(&streams.sockets, 7, &replacement));
        assert!(streams.get(7).is_none());
    }

    #[test]
    fn relay_stream_retirement_does_not_wait_for_transport_writer() {
        let streams = RelayStreams::default();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let socket = Arc::new(socket);
        let weak = Arc::downgrade(&socket);
        streams.insert(7, Arc::clone(&socket));
        let reader_streams = Arc::clone(&streams.sockets);
        let writer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_guard = writer.lock().unwrap();
        let reader_writer = Arc::clone(&writer);
        let (retired_tx, retired_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            assert!(retire_relay_stream(&reader_streams, 7, &socket));
            drop(socket);
            retired_tx.send(()).unwrap();
            write_relay_frame_locked(&reader_writer, RELAY_FRAME_CLOSE, 7, &[]).unwrap();
        });
        let retired = retired_rx.recv_timeout(Duration::from_secs(2));
        let reclaimed_before_unlock =
            retired.is_ok() && weak.upgrade().is_none() && streams.get(7).is_none();
        // Always release the writer and join, including a failed assertion path.
        drop(writer_guard);
        reader.join().unwrap();
        retired.unwrap();
        assert!(reclaimed_before_unlock);
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
