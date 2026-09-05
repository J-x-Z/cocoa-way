use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::container_mode;
use crate::container_sessions::{self, ContainerSession};
use crate::control_protocol::{ControlRequest, ControlResponse};
use crate::diagnostics;
use crate::messages::CompositorMessage;
use crate::runtime_paths::{build_child_path, find_command_path};

const CONTROL_SOCKET_ENV: &str = "COCOA_WAY_CONTROL_SOCKET";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

pub fn socket_path() -> PathBuf {
    std::env::var_os(CONTROL_SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("cocoa-way")
                .join("control")
                .join("control.sock")
        })
}

pub fn start(sender: Sender<CompositorMessage>) -> Result<PathBuf, String> {
    let path = socket_path();
    start_at(path, sender)
}

fn start_at(path: PathBuf, sender: Sender<CompositorMessage>) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "control socket path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create control socket directory: {error}"))?;
    let metadata = std::fs::metadata(parent)
        .map_err(|error| format!("failed to inspect control socket directory: {error}"))?;
    if metadata.uid() == unsafe { libc::geteuid() } {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("failed to secure control socket directory: {error}"))?;
    }
    if path.exists() && UnixStream::connect(&path).is_ok() {
        return Err(format!(
            "another Cocoa-Way control server is active at {}",
            path.display()
        ));
    }
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|error| format!("failed to replace stale control socket: {error}"))?;
    }
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("failed to bind control socket: {error}"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("failed to secure control socket: {error}"))?;

    let thread_path = path.clone();
    std::thread::Builder::new()
        .name("cocoa-way-control".into())
        .spawn(move || {
            for connection in listener.incoming() {
                match connection {
                    Ok(stream) => {
                        let connection_sender = sender.clone();
                        let connection_path = thread_path.clone();
                        let _ = std::thread::Builder::new()
                            .name("cocoa-way-control-request".into())
                            .spawn(move || {
                                handle_connection(stream, &connection_sender, &connection_path)
                            });
                    }
                    Err(error) => log::warn!("Control socket accept failed: {error}"),
                }
            }
        })
        .map_err(|error| format!("failed to start control socket thread: {error}"))?;
    Ok(path)
}

pub fn remove_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn handle_connection(mut stream: UnixStream, sender: &Sender<CompositorMessage>, path: &Path) {
    let request = {
        let mut line = String::new();
        let mut reader = BufReader::new((&stream).take(MAX_REQUEST_BYTES));
        match reader.read_line(&mut line) {
            Ok(0) => Err("empty control request".to_string()),
            Ok(_) => serde_json::from_str::<ControlRequest>(&line)
                .map_err(|error| format!("invalid control request: {error}")),
            Err(error) => Err(format!("failed to read control request: {error}")),
        }
    };
    let response = match request {
        Ok(request) => dispatch(request, sender, path),
        Err(error) => ControlResponse::failure("invalid", error),
    };
    if let Ok(mut encoded) = serde_json::to_vec(&response) {
        encoded.push(b'\n');
        let _ = stream.write_all(&encoded);
    }
}

fn dispatch(
    request: ControlRequest,
    sender: &Sender<CompositorMessage>,
    path: &Path,
) -> ControlResponse {
    let command = request.command.trim().to_ascii_lowercase();
    match command.as_str() {
        "status" => ControlResponse::success(&command, status_snapshot(path)),
        "applications" | "sessions" => ControlResponse::success(&command, sessions_snapshot()),
        "running" => ControlResponse::success(&command, active_sessions_json()),
        "displays" => ControlResponse::success(&command, displays_snapshot()),
        "images" => ControlResponse::success(&command, images_snapshot()),
        "volumes" => ControlResponse::success(&command, volumes_snapshot()),
        "runtimes" => ControlResponse::success(&command, runtimes_snapshot()),
        "tasks" => ControlResponse::success(
            &command,
            serde_json::to_value(container_mode::control_operation_tasks(
                request.limit.clamp(1, 1000),
            ))
            .unwrap_or_else(|error| json!({ "error": error.to_string() })),
        ),
        "environment" => ControlResponse::success(&command, environment_snapshot(path)),
        "features" => ControlResponse::success(&command, feature_matrix_snapshot()),
        "diagnostics" => ControlResponse::success(
            &command,
            diagnostics_snapshot(path, request.session.as_deref(), request.limit),
        ),
        "logs" => match resolve_session(request.session.as_deref()) {
            Ok((index, session)) => ControlResponse::success(
                &command,
                json!({
                    "index": index,
                    "name": session.name,
                    "lines": container_mode::control_session_logs(index, request.limit.clamp(1, 1000)),
                }),
            ),
            Err(error) => ControlResponse::failure(&command, error),
        },
        "launch" | "start" | "stop" | "check" => {
            queue_session_command(&command, request.session.as_deref(), sender)
        }
        "display-create" | "display-close" => {
            queue_display_command(&command, request.session.as_deref(), sender)
        }
        _ => ControlResponse::failure(
            &command,
            "unsupported command; use status, applications, running, displays, images, volumes, runtimes, tasks, environment, features, diagnostics, logs, check, launch, stop, display-create, or display-close",
        ),
    }
}

fn queue_session_command(
    command: &str,
    selector: Option<&str>,
    sender: &Sender<CompositorMessage>,
) -> ControlResponse {
    let (index, session) = match resolve_session(selector) {
        Ok(resolved) => resolved,
        Err(error) => return ControlResponse::failure(command, error),
    };
    let message = match command {
        "launch" | "start" => CompositorMessage::StartContainerSession(index),
        "stop" => CompositorMessage::StopContainerSession(index),
        "check" => CompositorMessage::CheckContainerSession(index),
        _ => unreachable!(),
    };
    match sender.send(message) {
        Ok(()) => ControlResponse::success(
            command,
            json!({
                "accepted": true,
                "index": index,
                "name": session.name,
                "note": "The command was queued on Cocoa-Way's compositor event loop.",
            }),
        ),
        Err(error) => {
            ControlResponse::failure(command, format!("event loop is unavailable: {error}"))
        }
    }
}

fn queue_display_command(
    command: &str,
    selector: Option<&str>,
    sender: &Sender<CompositorMessage>,
) -> ControlResponse {
    let requested = match selector
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(crate::normalize_managed_display_slot)
        .transpose()
    {
        Ok(requested) => requested,
        Err(error) => return ControlResponse::failure(command, error),
    };
    let (message, slot, note) = match command {
        "display-create" => (
            CompositorMessage::CreateManagedDisplay(requested.clone()),
            requested,
            "Display creation was queued on Cocoa-Way's compositor event loop.",
        ),
        "display-close" => {
            let Some(slot) = requested else {
                return ControlResponse::failure(command, "a managed display name is required");
            };
            (
                CompositorMessage::CloseManagedDisplay(slot.clone()),
                Some(slot),
                "Display closure was queued on Cocoa-Way's compositor event loop.",
            )
        }
        _ => unreachable!(),
    };
    let automatic = slot.is_none();
    match sender.send(message) {
        Ok(()) => ControlResponse::success(
            command,
            json!({
                "accepted": true,
                "slot": slot,
                "automatic": automatic,
                "note": note,
            }),
        ),
        Err(error) => {
            ControlResponse::failure(command, format!("event loop is unavailable: {error}"))
        }
    }
}

fn resolve_session(selector: Option<&str>) -> Result<(usize, ContainerSession), String> {
    let selector = selector
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "a session index or exact name is required".to_string())?;
    let sessions = container_sessions::load_sessions();
    if let Some((index, session)) = sessions
        .iter()
        .enumerate()
        .find(|(_, session)| session.name.eq_ignore_ascii_case(selector))
    {
        return Ok((index, session.clone()));
    }
    if let Ok(index) = selector.parse::<usize>()
        && let Some(session) = sessions.get(index)
    {
        return Ok((index, session.clone()));
    }
    Err(format!("session '{selector}' was not found"))
}

fn status_snapshot(path: &Path) -> Value {
    let active = active_sessions_json();
    let performance = container_mode::control_performance_snapshot().map(
        |(
            redraw_fps,
            commits_per_second,
            tiles,
            dirty,
            pending_frame_callbacks,
            late_redraws_per_second,
            max_redraw_wait_ms,
            input_to_present_ms,
        )| {
            json!({
                "redraw_fps": redraw_fps,
                "commits_per_second": commits_per_second,
                "tiles": tiles,
                "dirty": dirty,
                "pending_frame_callbacks": pending_frame_callbacks,
                "late_redraws_per_second": late_redraws_per_second,
                "max_redraw_wait_ms": max_redraw_wait_ms,
                "host_input_to_present_ms": input_to_present_ms,
            })
        },
    );
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "control_socket": path,
        "configured_sessions": container_sessions::load_sessions().len(),
        "active_sessions": active,
        "performance": performance,
        "resources": diagnostics::resource_snapshot(),
        "clipboard": diagnostics::clipboard_snapshot(),
        "audio": crate::audio::snapshot(),
        "activity": container_mode::control_activity_snapshot(10),
        "tasks": container_mode::control_operation_tasks(20),
    })
}

fn sessions_snapshot() -> Value {
    let active = container_mode::control_active_sessions();
    Value::Array(
        container_sessions::load_sessions()
            .into_iter()
            .enumerate()
            .map(|(index, session)| {
                let tracked = active.iter().find(|active| active.1 == index);
                let state = container_mode::control_session_state(index);
                json!({
                    "index": index,
                    "name": session.name,
                    "runtime": session.runtime,
                    "image": session.image,
                    "command": session.command,
                    "profile": session.profile,
                    "display": session.display.as_deref().unwrap_or("auto"),
                    "audio": session.audio,
                    "state": state.as_ref().map(|state| state.0.as_str()).unwrap_or(if tracked.is_some() { "Running" } else { "Idle" }),
                    "state_detail": state.map(|state| state.1),
                    "active": tracked.map(|active| json!({
                        "instance_id": active.0,
                        "started_at_unix_ms": active.2,
                        "container_pid": active.3,
                        "waypipe_pid": active.4,
                        "display_slot": active.5,
                        "display_pid": active.6,
                    })),
                })
            })
            .collect(),
    )
}

fn displays_snapshot() -> Value {
    json!({
        "default": {
            "kind": "embedded",
            "description": "The compositor window owned by the main Cocoa-Way process."
        },
        "active": active_sessions_json(),
        "performance": container_mode::control_display_performance()
            .into_iter()
            .map(|(slot, redraw_fps, commits_per_second, late_redraws_per_second, max_redraw_wait_ms, input_to_present_ms, sampled_at_unix_ms)| json!({
                "slot": slot,
                "redraw_fps": redraw_fps,
                "commits_per_second": commits_per_second,
                "late_redraws_per_second": late_redraws_per_second,
                "max_redraw_wait_ms": max_redraw_wait_ms,
                "host_input_to_present_ms": input_to_present_ms,
                "sampled_at_unix_ms": sampled_at_unix_ms,
            }))
            .collect::<Vec<_>>(),
        "managed": container_mode::control_managed_displays()
            .into_iter()
            .map(|(slot, status, runtime_dir, display, pid, attachments)| json!({
                "slot": slot,
                "status": status,
                "runtime_dir": runtime_dir,
                "wayland_display": display,
                "pid": pid,
                "attachments": attachments,
            }))
            .collect::<Vec<_>>(),
    })
}

fn active_sessions_json() -> Value {
    Value::Array(
        container_mode::control_active_sessions()
            .into_iter()
            .map(
                |(
                    instance_id,
                    profile_index,
                    started_at_unix_ms,
                    container_pid,
                    waypipe_pid,
                    display_slot,
                    display_pid,
                )| {
                    json!({
                        "instance_id": instance_id,
                        "profile_index": profile_index,
                        "started_at_unix_ms": started_at_unix_ms,
                        "container_pid": container_pid,
                        "waypipe_pid": waypipe_pid,
                        "display_slot": display_slot,
                        "display_pid": display_pid,
                    })
                },
            )
            .collect(),
    )
}

fn images_snapshot() -> Value {
    let child_path = build_child_path();
    json!({
        "apple_container": command_snapshot(
            "container",
            &["image", "list"],
            &child_path,
        ),
        "docker": command_snapshot(
            "docker",
            &["image", "ls", "--format", "{{json .}}"],
            &child_path,
        ),
    })
}

fn volumes_snapshot() -> Value {
    let child_path = build_child_path();
    json!({
        "apple_container": command_snapshot(
            "container",
            &["volume", "list", "--format", "json"],
            &child_path,
        ),
        "docker_compatible": command_snapshot(
            "docker",
            &["volume", "ls", "--format", "{{json .}}"],
            &child_path,
        ),
    })
}

fn runtimes_snapshot() -> Value {
    let child_path = build_child_path();
    json!({
        "operations": container_mode::control_runtime_states()
            .into_iter()
            .map(|(runtime, status, detail)| json!({
                "runtime": runtime,
                "status": status,
                "detail": detail,
            }))
            .collect::<Vec<_>>(),
        "apple_container": {
            "cli": command_probe("container", &["--version"], &child_path),
            "system": command_probe("container", &["system", "status"], &child_path),
        },
        "docker_compatible": {
            "cli": command_probe("docker", &["--version"], &child_path),
            "connection": command_probe("docker", &["info", "--format", "{{.ServerVersion}}"], &child_path),
            "contexts": command_snapshot("docker", &["context", "ls", "--format", "{{json .}}"], &child_path),
        },
        "orbstack_provider": {
            "cli": command_probe("orbctl", &["version"], &child_path),
            "status": command_probe("orbctl", &["status"], &child_path),
            "machines": command_snapshot("orbctl", &["list", "--format", "json"], &child_path),
        },
    })
}

fn environment_snapshot(path: &Path) -> Value {
    let child_path = build_child_path();
    redact_value(json!({
        "cocoa_way": {
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "control_socket": path,
            "config_path": container_sessions::config_path(),
            "config_exists": container_sessions::config_path().is_file(),
        },
        "host": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "macos_version": command_snapshot("sw_vers", &["-productVersion"], &child_path),
        },
        "commands": {
            "apple_container": command_probe("container", &["--version"], &child_path),
            "apple_container_system": command_probe("container", &["system", "status"], &child_path),
            "waypipe": command_probe("waypipe", &["--version"], &child_path),
            "docker": command_probe("docker", &["--version"], &child_path),
            "orbstack": command_probe("orbctl", &["version"], &child_path),
            "orbstack_status": command_probe("orbctl", &["status"], &child_path),
        },
    }))
}

fn feature_matrix_snapshot() -> Value {
    json!({
        "presentation": {
            "desktop": { "status": "supported", "note": "One compositor desktop per display slot." },
            "rootless": { "status": "experimental", "note": "Native macOS windows for regular xdg-shell applications; nested compositors are rejected." },
            "dedicated_displays": { "status": "supported", "note": "Automatic and manually named display slots are available." },
        },
        "transport": {
            "apple_container_socket_v2": { "status": "supported", "fallback": "stdio relay" },
            "classic_waypipe": { "status": "supported", "targets": ["SSH", "Docker", "OrbStack"] },
            "clipboard_text": { "status": "supported", "scope": "text MIME types" },
            "clipboard_image": { "status": "supported", "scope": "macOS to Wayland as image/png", "max_encoded_bytes": 67108864 },
            "audio": { "status": "supported_default_on", "format": "s16le/48000/2", "note": "Apple Container playback uses an independent published socket and macOS CoreAudio. Profiles can explicitly disable it; Metal rendering is unchanged." },
        },
        "runtime_control": {
            "apple_container": ["system", "containers", "images", "volumes"],
            "docker": ["contexts", "containers", "images"],
            "orbstack": ["system", "machines", "docker_compatible_containers"],
        },
        "automation": {
            "control_socket": "local Unix socket",
            "cli": "cocoa-wayctl",
            "mcp": { "status": "read_only", "destructive_tools": false },
        },
        "known_limits": [
            "Rootless Xwayland application projection is not available.",
            "Nested compositors such as niri and Hyprland require desktop presentation.",
            "Audio requires an image containing cocoa-way-audio-relay."
        ],
    })
}

fn diagnostics_snapshot(path: &Path, selector: Option<&str>, limit: usize) -> Value {
    let selected_logs = selector.map(|selector| match resolve_session(Some(selector)) {
        Ok((index, session)) => json!({
            "index": index,
            "name": session.name,
            "lines": container_mode::control_session_logs(index, limit.clamp(1, 1000)),
        }),
        Err(error) => json!({ "selector": selector, "error": error }),
    });
    redact_value(json!({
        "generated_at_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis()),
        "status": status_snapshot(path),
        "environment": environment_snapshot(path),
        "features": feature_matrix_snapshot(),
        "sessions": sessions_snapshot(),
        "displays": displays_snapshot(),
        "images": images_snapshot(),
        "volumes": volumes_snapshot(),
        "runtimes": runtimes_snapshot(),
        "selected_session_logs": selected_logs,
        "privacy": "Home-directory paths and the local account name are redacted before this snapshot is returned.",
    }))
}

fn command_probe(command: &str, args: &[&str], child_path: &str) -> Value {
    let Some(path) = find_command_path(command, child_path) else {
        return json!({ "available": false, "error": format!("{command} command not found") });
    };
    json!({
        "available": true,
        "path": path,
        "result": command_snapshot(command, args, child_path),
    })
}

fn redact_value(mut value: Value) -> Value {
    let home = std::env::var("HOME").ok().filter(|value| !value.is_empty());
    let user = std::env::var("USER").ok().filter(|value| !value.is_empty());
    redact_value_in_place(&mut value, home.as_deref(), user.as_deref());
    value
}

fn redact_value_in_place(value: &mut Value, home: Option<&str>, user: Option<&str>) {
    match value {
        Value::String(text) => {
            if let Some(home) = home {
                *text = text.replace(home, "~");
            }
            if let Some(user) = user {
                *text = text.replace(user, "<user>");
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value_in_place(value, home, user);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_value_in_place(value, home, user);
            }
        }
        _ => {}
    }
}

fn command_snapshot(command: &str, args: &[&str], child_path: &str) -> Value {
    let Some(path) = find_command_path(command, child_path) else {
        return json!({ "available": false, "error": format!("{command} command not found") });
    };
    match run_command(&path, args, child_path, Duration::from_secs(3)) {
        Ok(output) => json!({
            "available": true,
            "success": output.status.success(),
            "stdout": String::from_utf8_lossy(&output.stdout).lines().collect::<Vec<_>>(),
            "stderr": String::from_utf8_lossy(&output.stderr).lines().collect::<Vec<_>>(),
        }),
        Err(error) => json!({ "available": true, "success": false, "error": error }),
    }
}

fn run_command(
    path: &Path,
    args: &[&str],
    child_path: &str,
    timeout: Duration,
) -> Result<Output, String> {
    let mut child = Command::new(path)
        .env("PATH", child_path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|error| error.to_string()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("command timed out after {}ms", timeout.as_millis()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn control_socket_is_private_to_the_runtime_directory() {
        let path = socket_path();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("control.sock")
        );
        assert!(path.parent().is_some());
    }

    #[test]
    fn unsupported_control_command_returns_structured_failure() {
        let (sender, _receiver) = std::sync::mpsc::channel();
        let response = dispatch(
            ControlRequest {
                command: "delete".into(),
                session: None,
                limit: 10,
            },
            &sender,
            Path::new("/tmp/control.sock"),
        );
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("unsupported command"));
    }

    #[test]
    fn application_control_queries_are_structured_and_read_only() {
        let (sender, _receiver) = std::sync::mpsc::channel();
        for command in ["applications", "sessions", "running", "displays", "tasks"] {
            let response = dispatch(
                ControlRequest {
                    command: command.into(),
                    session: None,
                    limit: 10,
                },
                &sender,
                Path::new("/tmp/control.sock"),
            );
            assert!(response.ok, "{command} failed: {:?}", response.error);
            assert!(!response.data.is_null(), "{command} returned no data");
        }
    }

    #[test]
    fn display_control_commands_are_normalized_and_queued() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let create = dispatch(
            ControlRequest {
                command: "display-create".into(),
                session: Some("Research Window".into()),
                limit: 10,
            },
            &sender,
            Path::new("/tmp/control.sock"),
        );
        assert!(create.ok);
        assert_eq!(create.data["slot"], "research-window");
        match receiver.recv().unwrap() {
            CompositorMessage::CreateManagedDisplay(Some(slot)) => {
                assert_eq!(slot, "research-window")
            }
            _ => panic!("unexpected display-create message"),
        }

        let close = dispatch(
            ControlRequest {
                command: "display-close".into(),
                session: Some("Research Window".into()),
                limit: 10,
            },
            &sender,
            Path::new("/tmp/control.sock"),
        );
        assert!(close.ok);
        match receiver.recv().unwrap() {
            CompositorMessage::CloseManagedDisplay(slot) => assert_eq!(slot, "research-window"),
            _ => panic!("unexpected display-close message"),
        }
    }

    #[test]
    fn feature_matrix_keeps_automation_read_only() {
        let features = feature_matrix_snapshot();
        assert_eq!(features["automation"]["mcp"]["status"], "read_only");
        assert_eq!(features["automation"]["mcp"]["destructive_tools"], false);
    }

    #[test]
    fn feature_matrix_reports_audio_as_enabled_by_default() {
        let features = feature_matrix_snapshot();
        assert_eq!(
            features["transport"]["audio"]["status"],
            "supported_default_on"
        );
        assert!(
            !features["known_limits"][2]
                .as_str()
                .unwrap()
                .contains("enabled per session")
        );
    }

    #[test]
    fn feature_matrix_reports_bounded_host_image_clipboard() {
        let features = feature_matrix_snapshot();
        assert_eq!(
            features["transport"]["clipboard_image"]["scope"],
            "macOS to Wayland as image/png"
        );
        assert_eq!(
            features["transport"]["clipboard_image"]["max_encoded_bytes"],
            64 * 1024 * 1024
        );
    }

    #[test]
    fn diagnostics_redaction_covers_nested_values() {
        let mut value = json!({
            "path": "/Users/alice/.config/cocoa-way",
            "nested": ["owner=alice"]
        });
        redact_value_in_place(&mut value, Some("/Users/alice"), Some("alice"));
        assert_eq!(value["path"], "~/.config/cocoa-way");
        assert_eq!(value["nested"][0], "owner=<user>");
    }

    #[test]
    fn unix_socket_returns_a_json_status_response() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let (sender, _receiver) = std::sync::mpsc::channel();
        start_at(socket.clone(), sender).unwrap();

        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .write_all(b"{\"command\":\"status\",\"limit\":10}\n")
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        let response: ControlResponse = serde_json::from_str(&response).unwrap();
        assert!(response.ok);
        assert_eq!(response.command, "status");
        assert_eq!(
            response.data["control_socket"],
            socket.to_string_lossy().as_ref()
        );
    }
}
