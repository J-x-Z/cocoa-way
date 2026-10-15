use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::runtime_paths::{build_child_path, resolve_command_path};

static ASKPASS_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Connection {
    pub name: String,
    #[serde(rename = "type", default = "default_type")]
    pub conn_type: String, // "ssh" or "local"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>, // for conn_type = "local"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>, // program to launch on remote
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<String>,
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waypipe_path: Option<String>,
}

fn default_type() -> String {
    "ssh".into()
}

#[derive(Deserialize, Serialize, Default)]
struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waypipe_path: Option<String>,
    #[serde(default)]
    connection: Vec<Connection>,
}

pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/cocoa-way/connections.toml")
}

/// Load connections from ~/.config/cocoa-way/connections.toml.
/// Creates an example file if none exists.
pub fn load_connections() -> Vec<Connection> {
    let path = config_path();
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Write an example config so the user knows the format
            let _ = std::fs::create_dir_all(&config_dir);
            let example = r#"# cocoa-way connections
# Each [[connection]] block defines a remote machine to connect to via waypipe.
# Optional: set this if waypipe is installed somewhere cocoa-way cannot discover.
# waypipe_path = "/opt/homebrew/bin/waypipe"

# --- Local VM example (Unix socket) ---
# [[connection]]
# name = "Linux VM"
# type = "local"
# socket = "/tmp/waypipe-vm.sock"
# app = "weston-terminal"
# display = "default"

# --- Remote SSH example ---
# [[connection]]
# name = "Home Server"
# type = "ssh"
# host = "192.168.1.100"
# user = "jiaxi"
# app = "weston-terminal"
# port = 22
# identity = "~/.ssh/id_rsa"
# display = "display-1"
# compression = "zstd"
"#;
            let _ = std::fs::write(&path, example);
            log::info!("Created example connections.toml at {:?}", path);
            return vec![];
        }
        Err(error) => {
            log::error!(
                "Could not read saved connections from {}: {error}. Check the file permissions; Cocoa-Way has not replaced the file.",
                path.display()
            );
            return vec![];
        }
    };

    match toml::from_str::<Config>(&content) {
        Ok(mut cfg) => {
            for conn in &mut cfg.connection {
                if conn.waypipe_path.is_none() {
                    conn.waypipe_path = cfg.waypipe_path.clone();
                }
            }
            cfg.connection
        }
        Err(e) => {
            log::warn!(
                "Failed to parse {}: {}. Correct this TOML file before using saved connections; Cocoa-Way has not replaced it.",
                path.display(),
                e
            );
            vec![]
        }
    }
}

/// Persist a reusable connection without storing its password.
///
/// Connections are updated by name so saving the same entry again does not
/// create duplicate menu items. The returned index matches the menu tag used
/// after the connection menu is rebuilt.
pub fn save_connection(connection: &Connection) -> Result<usize, String> {
    let path = config_path();
    save_connection_at(&path, connection)
}

fn save_connection_at(path: &Path, connection: &Connection) -> Result<usize, String> {
    let name = connection.name.trim();
    if name.is_empty() {
        return Err("a saved connection requires a name".into());
    }
    if connection.conn_type == "ssh"
        && connection
            .host
            .as_deref()
            .map_or(true, |host| host.trim().is_empty())
    {
        return Err("an SSH connection requires a host".into());
    }

    let mut config = match fs::read_to_string(path) {
        Ok(content) => toml::from_str::<Config>(&content)
            .map_err(|error| format!("failed to parse {}: {}", path.display(), error))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => return Err(format!("failed to read {}: {}", path.display(), error)),
    };

    let mut stored = connection.clone();
    stored.name = name.to_string();
    stored.password = None;
    if stored.waypipe_path == config.waypipe_path {
        stored.waypipe_path = None;
    }

    let index = if let Some(index) = config
        .connection
        .iter()
        .position(|existing| existing.name == stored.name)
    {
        config.connection[index] = stored;
        index
    } else {
        let index = config.connection.len();
        config.connection.push(stored);
        index
    };

    let serialized = toml::to_string_pretty(&config)
        .map_err(|error| format!("failed to serialize connection config: {}", error))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {}", parent.display(), error))?;
    let temporary = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    let write_result = (|| {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(b"# Cocoa-Way saved connections. Passwords are never stored.\n")?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(format!("failed to save {}: {}", path.display(), error));
    }
    Ok(index)
}

/// Spawn a waypipe connection for the given connection config.
/// Returns the child process handle so the caller can track/kill it.
pub fn spawn_waypipe(conn: &Connection, runtime_dir: &str, display: &str) -> Result<Child, String> {
    let app = conn.app.as_deref().unwrap_or("weston-terminal");
    let waypipe = resolve_waypipe_path(conn.waypipe_path.as_deref())
        .ok_or_else(|| "Waypipe was not found in PATH or the configured executable path. Install it with `brew install J-x-Z/tap/waypipe-darwin`, or correct waypipe_path in Connections settings. Waypipe is also required on the Linux host.".to_string())?;
    let child_path = build_child_path();
    let target = resolve_display_target(conn.display.as_deref(), runtime_dir, display)?;

    match conn.conn_type.as_str() {
        "local" => {
            // Local VM reachable through a Unix socket (e.g. OrbStack / QEMU)
            let socket = conn
                .socket
                .as_deref()
                .filter(|socket| !socket.trim().is_empty())
                .ok_or_else(|| "local connections require a Unix socket path".to_string())?;
            spawn_local_waypipe_client(
                &waypipe,
                &child_path,
                &target.runtime_dir,
                &target.display,
                socket,
            )
        }
        _ => {
            // SSH connection
            let host = conn
                .host
                .as_deref()
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .ok_or_else(|| "SSH connections require a host".to_string())?;
            let ssh_target = conn
                .user
                .as_deref()
                .map(str::trim)
                .filter(|user| !user.is_empty())
                .map(|user| format!("{}@{}", user, host))
                .unwrap_or_else(|| host.to_string());
            let compression = conn
                .compression
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("zstd");

            let mut args: Vec<String> = vec![
                format!("--compress={}", compression),
                "ssh".into(),
                "-o".into(),
                "StreamLocalBindUnlink=yes".into(),
            ];
            if let Some(port) = conn.port {
                args.extend(["-p".into(), port.to_string()]);
            }
            if let Some(identity) = &conn.identity {
                args.extend(["-i".into(), identity.clone()]);
            }
            args.push(ssh_target);
            args.push(app.into());

            let mut cmd = Command::new(&waypipe);
            cmd.env("PATH", &child_path)
                .env("XDG_RUNTIME_DIR", &target.runtime_dir)
                .env("WAYLAND_DISPLAY", &target.display)
                .args(&args);

            if let Some(pw) = &conn.password {
                spawn_with_askpass(&mut cmd, pw)
            } else {
                cmd.spawn()
                    .map_err(|error| format!("failed to start waypipe SSH connection: {}", error))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayTarget {
    runtime_dir: String,
    display: String,
}

fn resolve_display_target(
    requested_slot: Option<&str>,
    fallback_runtime_dir: &str,
    fallback_display: &str,
) -> Result<DisplayTarget, String> {
    let requested_slot = requested_slot
        .map(str::trim)
        .filter(|slot| !slot.is_empty());
    if requested_slot.is_none()
        || requested_slot == Some("default")
        || requested_slot == Some("auto")
    {
        return validate_display_target(fallback_runtime_dir, fallback_display, "default");
    }

    let requested_slot = requested_slot.unwrap();
    for entry in fs::read_dir("/tmp").into_iter().flatten().flatten() {
        let runtime_dir = entry.path();
        let Some(name) = runtime_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("cwd-") {
            continue;
        }
        let Some(directory) = TrustedDisplayDirectory::open(&runtime_dir) else {
            continue;
        };
        let Some(slot) = directory.marker("display.slot") else {
            continue;
        };
        if slot.trim() != requested_slot || !directory.managed_is_live() {
            continue;
        }
        if let Some(display) = find_wayland_socket(&runtime_dir, &directory) {
            return Ok(DisplayTarget {
                runtime_dir: runtime_dir.display().to_string(),
                display,
            });
        }
    }

    Err(format!(
        "display slot '{}' is not available; create it in Container Mode or use run_waypipe.sh --list-displays",
        requested_slot
    ))
}

fn validate_display_target(
    runtime_dir: &str,
    display: &str,
    slot: &str,
) -> Result<DisplayTarget, String> {
    let runtime_dir = runtime_dir.trim();
    let display = display.trim();
    if runtime_dir.is_empty() || display.is_empty() {
        return Err(format!(
            "Cocoa-Way display '{}' is not ready; start its display window first",
            slot
        ));
    }
    let socket = Path::new(runtime_dir).join(display);
    if fs::symlink_metadata(&socket).is_err() {
        return Err(format!(
            "Cocoa-Way display socket '{}' does not exist",
            socket.display()
        ));
    }
    let directory = TrustedDisplayDirectory::open(Path::new(runtime_dir))
        .filter(|directory| {
            directory.socket(display) && directory.still_current(Path::new(runtime_dir))
        })
        .ok_or_else(|| {
            format!(
                "Cocoa-Way display '{}' has unsafe ownership, permissions, or file type",
                socket.display()
            )
        })?;
    drop(directory);
    Ok(DisplayTarget {
        runtime_dir: runtime_dir.to_string(),
        display: display.to_string(),
    })
}

const MAX_DISPLAY_MARKER_BYTES: u64 = 256;

fn trusted_metadata(metadata: &fs::Metadata, uid: u32, directory: bool) -> bool {
    metadata.uid() == uid && metadata.mode() & (if directory { 0o077 } else { 0o022 }) == 0
}

// Pin the directory and inspect children relative to it. Never open a FIFO in blocking mode.
pub(crate) struct TrustedDisplayDirectory(fs::File);

impl TrustedDisplayDirectory {
    pub(crate) fn open(path: &Path) -> Option<Self> {
        let path = path.components().collect::<PathBuf>();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for ancestor in parent
            .ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
        {
            let metadata = fs::symlink_metadata(ancestor).ok()?;
            // Only root-owned system aliases (not user-controlled redirects) may be followed.
            if metadata.file_type().is_symlink() {
                if metadata.uid() != 0 {
                    return None;
                }
            } else if !metadata.is_dir()
                || (metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() })
                || (metadata.mode() & 0o022 != 0
                    && !(metadata.uid() == 0 && metadata.mode() & 0o1000 != 0))
            {
                return None;
            }
        }
        let parent = fs::canonicalize(parent).ok()?;
        for ancestor in parent.ancestors() {
            let metadata = fs::symlink_metadata(ancestor).ok()?;
            let uid = metadata.uid();
            if !metadata.is_dir()
                || (uid != 0 && uid != unsafe { libc::geteuid() })
                || (metadata.mode() & 0o022 != 0 && !(uid == 0 && metadata.mode() & 0o1000 != 0))
            {
                return None;
            }
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        (metadata.is_dir() && trusted_metadata(&metadata, unsafe { libc::geteuid() }, true))
            .then_some(Self(file))
    }

    pub(crate) fn marker(&self, name: &str) -> Option<String> {
        if !matches!(name, "display.slot" | "display.parent" | "display.worker") {
            return None;
        }
        let name = std::ffi::CString::new(name).ok()?;
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let metadata = file.metadata().ok()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || !trusted_metadata(&metadata, unsafe { libc::geteuid() }, false)
            || metadata.len() > MAX_DISPLAY_MARKER_BYTES
        {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_DISPLAY_MARKER_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_DISPLAY_MARKER_BYTES {
            return None;
        }
        String::from_utf8(bytes).ok()
    }

    fn socket(&self, name: &str) -> bool {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return false;
        }
        let Ok(name) = std::ffi::CString::new(name) else {
            return false;
        };
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                &mut metadata,
                libc::AT_SYMLINK_NOFOLLOW,
            ) == 0
        }) && metadata.st_mode & libc::S_IFMT == libc::S_IFSOCK
            && metadata.st_uid == unsafe { libc::geteuid() }
            && metadata.st_mode & 0o022 == 0
    }

    fn socket_names(&self) -> Option<Vec<String>> {
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return None;
        }
        let directory = unsafe { libc::fdopendir(fd) };
        if directory.is_null() {
            unsafe { libc::close(fd) };
            return None;
        }
        struct Entries(*mut libc::DIR);
        impl Drop for Entries {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }
        let entries = Entries(directory);
        let mut names = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(entries.0) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            if let Ok(name) = name.to_str()
                && name.starts_with("wayland-")
                && self.socket(name)
            {
                names.push(name.to_string());
            }
        }
        Some(names)
    }

    pub(crate) fn still_current(&self, path: &Path) -> bool {
        let path = path.components().collect::<PathBuf>();
        self.0
            .metadata()
            .ok()
            .zip(fs::symlink_metadata(&path).ok())
            .is_some_and(|(opened, current)| {
                current.is_dir()
                    && opened.dev() == current.dev()
                    && opened.ino() == current.ino()
                    && trusted_metadata(&current, unsafe { libc::geteuid() }, true)
            })
    }

    fn managed_is_live(&self) -> bool {
        ["display.parent", "display.worker"].iter().all(|name| {
            self.marker(name)
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
                .is_some_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0)
        })
    }
}

fn find_wayland_socket(runtime_dir: &Path, directory: &TrustedDisplayDirectory) -> Option<String> {
    let mut sockets = directory.socket_names()?;
    sockets.sort();
    directory
        .still_current(runtime_dir)
        .then(|| sockets.into_iter().next())
        .flatten()
}

fn resolve_waypipe_path(configured: Option<&str>) -> Option<PathBuf> {
    let child_path = build_child_path();
    resolve_command_path("waypipe", configured, "waypipe", &child_path)
}

fn spawn_local_waypipe_client(
    waypipe: &Path,
    child_path: &str,
    runtime_dir: &str,
    display: &str,
    socket: &str,
) -> Result<Child, String> {
    Command::new(waypipe)
        .env("PATH", child_path)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", display)
        .args(["--socket", socket, "client"])
        .spawn()
        .map_err(|error| format!("failed to start local waypipe client: {}", error))
}

/// Reuse the current binary as an in-memory SSH_ASKPASS helper.
fn spawn_with_askpass(cmd: &mut Command, password: &str) -> Result<Child, String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    let helper = std::env::current_exe()
        .map_err(|error| format!("failed to locate Cocoa-Way for SSH askpass: {}", error))?;
    let sequence = ASKPASS_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let socket = PathBuf::from(format!(
        "/tmp/cocoa-way-askpass-{}-{}.sock",
        std::process::id(),
        sequence
    ));
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("failed to create private SSH askpass socket: {}", error))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("failed to secure SSH askpass socket: {}", error))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("failed to configure SSH askpass socket: {}", error))?;

    let secret = password.to_string();
    let cleanup_socket = socket.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.write_all(secret.as_bytes());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(cleanup_socket);
    });

    cmd.env("SSH_ASKPASS", helper)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("COCOA_WAY_ASKPASS_SOCKET", &socket);
    cmd.spawn()
        .map_err(|error| format!("failed to start waypipe password connection: {}", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn display_markers_reject_fifo_symlink_oversize_and_writable_files() {
        use std::os::unix::fs::symlink;
        let temp = private_directory();
        let directory = TrustedDisplayDirectory::open(temp.path()).unwrap();
        let marker = temp.path().join("display.slot");
        let cpath = std::ffi::CString::new(marker.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(directory.marker("display.slot").is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        fs::remove_file(&marker).unwrap();
        let other = temp.path().join("other");
        fs::write(&other, "display-1").unwrap();
        symlink(&other, &marker).unwrap();
        assert!(directory.marker("display.slot").is_none());
        fs::remove_file(&marker).unwrap();
        fs::write(&marker, vec![b'x'; MAX_DISPLAY_MARKER_BYTES as usize + 1]).unwrap();
        assert!(directory.marker("display.slot").is_none());
        fs::write(&marker, "display-1\n").unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(directory.marker("display.slot").is_none());
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            directory.marker("display.slot").as_deref(),
            Some("display-1\n")
        );
    }

    #[test]
    fn display_discovery_rejects_unsafe_directories_and_non_socket_targets() {
        use std::os::unix::fs::symlink;
        let temp = private_directory();
        let runtime = temp.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let directory = TrustedDisplayDirectory::open(&runtime).unwrap();
        fs::write(runtime.join("wayland-0"), "not a socket").unwrap();
        let socket = runtime.join("wayland-1");
        let _listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&socket, runtime.join("wayland-00")).unwrap();
        assert_eq!(
            find_wayland_socket(&runtime, &directory).as_deref(),
            Some("wayland-1")
        );
        assert!(!directory.socket("../runtime/wayland-1"));
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(find_wayland_socket(&runtime, &directory).is_none());
        let alias = temp.path().join("alias");
        symlink(&runtime, &alias).unwrap();
        assert!(TrustedDisplayDirectory::open(&alias).is_none());
        assert!(
            TrustedDisplayDirectory::open(Path::new(&format!("{}/", alias.display()))).is_none()
        );
        let nested = runtime.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(TrustedDisplayDirectory::open(&alias.join("nested")).is_none());
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(TrustedDisplayDirectory::open(&runtime).is_none());
        assert!(!directory.still_current(&runtime));
    }

    #[test]
    fn trusted_metadata_checks_the_expected_owner_without_chown() {
        let temp = private_directory();
        let metadata = fs::symlink_metadata(temp.path()).unwrap();
        assert!(trusted_metadata(&metadata, metadata.uid(), true));
        assert!(!trusted_metadata(
            &metadata,
            metadata.uid().wrapping_add(1),
            true
        ));
    }

    #[test]
    fn replacing_the_runtime_path_with_a_fifo_does_not_affect_pinned_discovery() {
        let temp = private_directory();
        let runtime = temp.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let _listener = UnixListener::bind(runtime.join("wayland-1")).unwrap();
        let directory = TrustedDisplayDirectory::open(&runtime).unwrap();
        fs::rename(&runtime, temp.path().join("moved")).unwrap();
        let cpath = std::ffi::CString::new(runtime.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(find_wayland_socket(&runtime, &directory).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn managed_displays_require_both_bounded_live_pid_markers() {
        let temp = private_directory();
        let directory = TrustedDisplayDirectory::open(temp.path()).unwrap();
        fs::write(
            temp.path().join("display.parent"),
            std::process::id().to_string(),
        )
        .unwrap();
        assert!(!directory.managed_is_live());
        let worker = temp.path().join("display.worker");
        fs::write(&worker, "0").unwrap();
        assert!(!directory.managed_is_live());
        fs::write(&worker, std::process::id().to_string()).unwrap();
        assert!(directory.managed_is_live());
    }

    #[test]
    fn default_display_requires_a_live_socket() {
        let temp = private_directory();
        let socket = temp.path().join("wayland-1");
        let _listener = UnixListener::bind(&socket).unwrap();
        let target =
            resolve_display_target(Some("default"), temp.path().to_str().unwrap(), "wayland-1")
                .unwrap();
        assert_eq!(target.display, "wayland-1");
    }

    #[test]
    fn missing_default_display_is_reported() {
        let error = resolve_display_target(Some("default"), "/missing", "wayland-1").unwrap_err();
        assert!(error.contains("does not exist"));
    }

    #[test]
    fn ssh_launch_matches_the_script_defaults() {
        let temp = private_directory();
        let socket = temp.path().join("wayland-1");
        let _listener = UnixListener::bind(&socket).unwrap();
        let capture = temp.path().join("capture.txt");
        let fake_waypipe = temp.path().join("waypipe");
        fs::write(
            &fake_waypipe,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$XDG_RUNTIME_DIR\" \"$WAYLAND_DISPLAY\" \"$@\" > '{}'\n",
                capture.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_waypipe, fs::Permissions::from_mode(0o755)).unwrap();

        let connection = Connection {
            name: "test".into(),
            conn_type: "ssh".into(),
            host: Some("linux.example".into()),
            user: None,
            port: None,
            identity: None,
            socket: None,
            app: Some("niri".into()),
            display: Some("auto".into()),
            compression: None,
            password: None,
            waypipe_path: Some(fake_waypipe.display().to_string()),
        };
        let mut child =
            spawn_waypipe(&connection, temp.path().to_str().unwrap(), "wayland-1").unwrap();
        assert!(child.wait().unwrap().success());
        let captured = fs::read_to_string(capture).unwrap();
        assert!(captured.contains("--compress=zstd"));
        assert!(captured.contains("StreamLocalBindUnlink=yes"));
        assert!(captured.contains("linux.example"));
        assert!(!captured.contains("root@linux.example"));
        assert!(captured.contains("niri"));
    }

    #[test]
    fn saved_connections_replace_by_name_and_never_store_passwords() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("connections.toml");
        let mut connection = Connection {
            name: "Lab".into(),
            conn_type: "ssh".into(),
            host: Some("linux.example".into()),
            user: Some("dev".into()),
            port: Some(22),
            identity: None,
            socket: None,
            app: Some("niri".into()),
            display: Some("auto".into()),
            compression: None,
            password: Some("not-on-disk".into()),
            waypipe_path: None,
        };

        assert_eq!(save_connection_at(&path, &connection).unwrap(), 0);
        connection.app = Some("foot".into());
        assert_eq!(save_connection_at(&path, &connection).unwrap(), 0);

        let content = fs::read_to_string(&path).unwrap();
        let config: Config = toml::from_str(&content).unwrap();
        assert_eq!(config.connection.len(), 1);
        assert_eq!(config.connection[0].app.as_deref(), Some("foot"));
        assert!(!content.contains("not-on-disk"));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
