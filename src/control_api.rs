use std::cell::Cell;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::container_mode;
use crate::container_sessions::{self, ContainerSession};
#[allow(dead_code)]
#[path = "control_client.rs"]
mod control_io;
use crate::control_protocol::{ControlRequest, ControlResponse};
use crate::diagnostics;
use crate::event_loop_signal::Sender;
use crate::messages::CompositorMessage;
use crate::runtime_paths::{build_child_path, find_command_path};
use control_io::{
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, connect_before, read_frame_before, write_before,
};

const CONTROL_SOCKET_ENV: &str = "COCOA_WAY_CONTROL_SOCKET";
const MAX_ACTIVE_HANDLERS: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE_TIMEOUT: Duration = Duration::from_millis(100);

thread_local! {
    // Inventory probes share the connection budget rather than each starting a new one.
    static REQUEST_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

struct HandlerPermit(Arc<AtomicUsize>);

impl HandlerPermit {
    fn acquire(active: &Arc<AtomicUsize>) -> Option<Self> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_ACTIVE_HANDLERS).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(active.clone()))
    }
}

impl Drop for HandlerPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn spawn_connection(
    stream: UnixStream,
    sender: Sender<CompositorMessage>,
    path: PathBuf,
    active: &Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let permit = HandlerPermit::acquire(active).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::WouldBlock, "control server is busy")
    })?;
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    std::thread::Builder::new()
        .name("cocoa-way-control-request".into())
        .spawn(move || {
            let _permit = permit;
            handle_connection(stream, &sender, &path, deadline);
        })?;
    Ok(())
}

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
    if path.exists() {
        match connect_before(&path, Instant::now() + WRITE_TIMEOUT) {
            Ok(_) => {
                return Err(format!(
                    "another Cocoa-Way control server is active at {}",
                    path.display()
                ));
            }
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                return Err(format!(
                    "cannot safely replace control socket {}: {error}",
                    path.display()
                ));
            }
            Err(_) => {}
        }
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
            let active = Arc::new(AtomicUsize::new(0));
            for connection in listener.incoming() {
                match connection {
                    Ok(stream) => {
                        if let Err(error) =
                            spawn_connection(stream, sender.clone(), thread_path.clone(), &active)
                            && error.kind() != std::io::ErrorKind::WouldBlock
                        {
                            log::warn!("Control request worker failed: {error}");
                        }
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

fn handle_connection(
    stream: UnixStream,
    sender: &Sender<CompositorMessage>,
    path: &Path,
    deadline: Instant,
) {
    handle_connection_with(stream, sender, path, deadline, dispatch);
}

fn collection_deadline(deadline: Instant, now: Instant) -> Instant {
    let remaining = deadline.saturating_duration_since(now);
    deadline - WRITE_TIMEOUT.min(remaining / 4)
}

fn handle_connection_with(
    mut stream: UnixStream,
    sender: &Sender<CompositorMessage>,
    path: &Path,
    deadline: Instant,
    collect: impl FnOnce(ControlRequest, &Sender<CompositorMessage>, &Path) -> ControlResponse,
) {
    // Collection and subprocess cleanup must leave time to return a partial
    // diagnostic report. The overall I/O deadline is never extended.
    let collection_deadline = collection_deadline(deadline, Instant::now());
    REQUEST_DEADLINE.with(|budget| budget.set(Some(collection_deadline)));
    let request = read_frame_before(
        &mut stream,
        MAX_REQUEST_BYTES,
        collection_deadline.min(Instant::now() + READ_TIMEOUT),
    )
    .map_err(|error| format!("failed to read control request: {error}"))
    .and_then(|line| {
        serde_json::from_slice::<ControlRequest>(&line)
            .map_err(|error| format!("invalid control request: {error}"))
    });
    let response = match request {
        Ok(request) if Instant::now() < collection_deadline => collect(request, sender, path),
        Ok(_) => ControlResponse::failure("invalid", "control request deadline exceeded"),
        Err(error) => ControlResponse::failure("invalid", error),
    };
    if let Ok(mut encoded) = serde_json::to_vec(&response) {
        if encoded.len() > MAX_RESPONSE_BYTES {
            encoded = serde_json::to_vec(&ControlResponse::failure(
                &response.command,
                "control response exceeds the 4 MiB frame limit",
            ))
            .unwrap_or_default();
        }
        encoded.push(b'\n');
        let _ = write_before(
            &mut stream,
            &encoded,
            deadline.min(Instant::now() + WRITE_TIMEOUT),
        );
    }
    REQUEST_DEADLINE.with(|budget| budget.set(None));
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
        "debug-snapshot" => ControlResponse::success(&command, debug_snapshot(path)),
        "performance" => ControlResponse::success(&command, performance_snapshot(path)),
        "clipboard" => ControlResponse::success(&command, json!(diagnostics::clipboard_snapshot())),
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
                    "id": session.id,
                    "index": index,
                    "name": session.name,
                    "lines": container_mode::control_session_logs(session.id, request.limit.clamp(1, 1000)),
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
        "open-panel" => queue_panel_command(request.session.as_deref(), sender),
        _ => ControlResponse::failure(
            &command,
            "unsupported command; use status, applications, running, displays, debug-snapshot, performance, clipboard, images, volumes, runtimes, tasks, environment, features, diagnostics, logs, check, launch, stop, display-create, display-close, or open-panel",
        ),
    }
}

/// Admission is not acceptance until the message is actually queued. A stalled
/// UI must not retain every control handler beyond its request's I/O deadline.
fn queue_message(
    sender: &Sender<CompositorMessage>,
    message: CompositorMessage,
) -> Result<(), &'static str> {
    let timeout = REQUEST_DEADLINE.with(|budget| match budget.get() {
        Some(deadline) => deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(QUEUE_TIMEOUT))
            .ok_or("control request deadline exceeded"),
        None => Ok(QUEUE_TIMEOUT),
    })?;
    sender
        .send_timeout(message, timeout)
        .map_err(|error| match error {
            crate::event_loop_signal::SendTimeoutError::Timeout(_) => {
                "event loop queue admission timed out; command was not accepted"
            }
            crate::event_loop_signal::SendTimeoutError::Disconnected(_) => {
                "event loop is unavailable; command was not accepted"
            }
        })
}

fn queue_session_command(
    command: &str,
    selector: Option<&str>,
    sender: &Sender<CompositorMessage>,
) -> ControlResponse {
    if command == "stop" {
        let id = match stop_profile_id(selector, resolve_session) {
            Ok(id) => id,
            Err(error) => return ControlResponse::failure(command, error),
        };
        return match queue_message(sender, CompositorMessage::StopContainerSession(id)) {
            Ok(()) => ControlResponse::success(
                command,
                json!({
                    "accepted": true,
                    "id": id,
                    "note": "Stop was queued by persistent ID; configuration is not required to release an owned instance.",
                }),
            ),
            Err(error) => ControlResponse::failure(command, error),
        };
    }
    let (index, session) = match resolve_session(selector) {
        Ok(resolved) => resolved,
        Err(error) => return ControlResponse::failure(command, error),
    };
    let message = session_command_message(command, &session);
    match queue_message(sender, message) {
        Ok(()) => ControlResponse::success(
            command,
            json!({
                "accepted": true,
                "id": session.id,
                "index": index,
                "name": session.name,
                "note": "The command was queued on Cocoa-Way's compositor event loop.",
            }),
        ),
        Err(error) => ControlResponse::failure(command, error),
    }
}

fn stop_profile_id(
    selector: Option<&str>,
    resolve: impl FnOnce(Option<&str>) -> Result<(usize, ContainerSession), String>,
) -> Result<container_sessions::ProfileId, String> {
    if let Some(id) = selector.and_then(|selector| selector.trim().parse().ok()) {
        return Ok(id);
    }
    resolve(selector).map(|(_, session)| session.id)
}

fn queue_panel_command(page: Option<&str>, sender: &Sender<CompositorMessage>) -> ControlResponse {
    let page = page.unwrap_or("applications");
    let target = match page {
        "applications" => container_mode::PanelPage::Applications,
        "displays" => container_mode::PanelPage::Displays,
        "diagnostics" => container_mode::PanelPage::Diagnostics,
        _ => {
            return ControlResponse::failure(
                "open-panel",
                "page must be applications, displays, or diagnostics",
            );
        }
    };
    match queue_message(sender, CompositorMessage::OpenPanel(target)) {
        Ok(()) => ControlResponse::success(
            "open-panel",
            json!({
                "accepted": true,
                "page": page,
                "note": "Panel opening was queued; no application or container is started.",
            }),
        ),
        Err(error) => ControlResponse::failure("open-panel", error),
    }
}

fn session_command_message(command: &str, session: &ContainerSession) -> CompositorMessage {
    match command {
        "launch" | "start" => CompositorMessage::StartContainerSessionIfCurrent {
            profile_id: session.id,
            signature: container_sessions::session_to_toml(&session),
        },
        "stop" => CompositorMessage::StopContainerSession(session.id),
        "check" => CompositorMessage::CheckContainerSession(session.id),
        _ => unreachable!(),
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
    match queue_message(sender, message) {
        Ok(()) => ControlResponse::success(
            command,
            json!({
                "accepted": true,
                "slot": slot,
                "automatic": automatic,
                "note": note,
            }),
        ),
        Err(error) => ControlResponse::failure(command, error),
    }
}

fn resolve_session(selector: Option<&str>) -> Result<(usize, ContainerSession), String> {
    let report = inspect_profile_config();
    if let Some(error) = report.error {
        return Err(error);
    }
    let resolved = resolve_session_in(selector, &report.sessions)?;
    if !resolved.1.id.is_saved() {
        return Err("This legacy profile needs a persistent ID. Save it in Cocoa-Way before launching or checking it.".into());
    }
    Ok(resolved)
}

fn inspect_profile_config() -> container_sessions::SessionLoadReport {
    inspect_profile_config_at(&container_sessions::config_path())
}

fn inspect_profile_config_at(path: &Path) -> container_sessions::SessionLoadReport {
    match container_sessions::inspect_sessions(path) {
        Ok(sessions) => container_sessions::SessionLoadReport {
            sessions,
            error: None,
        },
        Err(error) => container_sessions::SessionLoadReport {
            sessions: Vec::new(),
            error: Some(format!("Cannot read application configuration: {error}")),
        },
    }
}

fn resolve_session_in(
    selector: Option<&str>,
    sessions: &[ContainerSession],
) -> Result<(usize, ContainerSession), String> {
    let selector = selector
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "a profile ID, session index or exact name is required".to_string())?;
    if let Ok(id) = selector.parse::<container_sessions::ProfileId>() {
        return sessions
            .iter()
            .enumerate()
            .find(|(_, session)| session.id == id)
            .map(|(index, session)| (index, session.clone()))
            .ok_or_else(|| format!("profile '{id}' was not found"));
    }
    if sessions
        .iter()
        .filter(|session| session.name.eq_ignore_ascii_case(selector))
        .count()
        > 1
    {
        return Err("session name is ambiguous; use its persistent profile ID".into());
    }
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
    let config = inspect_profile_config();
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
                "host_input_to_submit_ms": input_to_present_ms,
                "latency_measurement": "input-to-CPU-submit estimate; present_ms is a legacy alias, not input-to-photon",
            })
        },
    );
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "control_socket": path,
        "configured_sessions": config.sessions.len(),
        "configuration_error": config.error,
        "configuration_migration_required": config.sessions.iter().any(|session| !session.id.is_saved()),
        "active_sessions": active,
        "performance": performance,
        "resources": diagnostics::resource_snapshot(),
        "clipboard": diagnostics::clipboard_snapshot(),
        "audio": crate::audio::snapshot(),
        "activity": container_mode::control_activity_snapshot(10),
        "tasks": container_mode::control_operation_tasks(20),
    })
}

fn debug_snapshot(path: &Path) -> Value {
    // Cached state only: unlike full diagnostics, this never probes a runtime
    // daemon or starts a subprocess, so it is safe to poll during input testing.
    redact_value(json!({
        "generated_at_unix_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis()),
        "status": status_snapshot(path),
        "applications": sessions_snapshot(),
        "displays": displays_snapshot(),
        "features": feature_matrix_snapshot(),
        "collection": "cached state and read-only configuration; no runtime subprocesses or network probes",
        "privacy": "Diagnostic redaction is applied; clipboard contents are never included. Review before sharing.",
    }))
}

fn performance_snapshot(path: &Path) -> Value {
    let status = status_snapshot(path);
    let displays = displays_snapshot();
    redact_value(json!({
        "host": status["performance"],
        "displays": displays["performance"],
        "cursor_paths": displays["cursor_paths"],
        "resources": status["resources"],
        "measurement_limits": {
            "input_latency": "Host input to CPU submit is an estimate, not screen presentation or input-to-photon latency.",
            "guest_clock": "No synchronized Guest receive timestamps are available in this snapshot.",
            "percentiles": "A snapshot does not establish P50/P95/P99; collect correlated trace samples for a latency distribution.",
            "freshness": "Per-display and resource entries include their own sampling timestamps. Idle displays may report zero FPS.",
        },
    }))
}

fn sessions_snapshot() -> Value {
    let config = inspect_profile_config();
    if let Some(error) = config.error {
        return json!({ "error": error, "sessions": [] });
    }
    let active = container_mode::control_active_sessions();
    Value::Array(
        config.sessions
            .into_iter()
            .enumerate()
            .map(|(index, session)| {
                let tracked = active.iter().find(|active| active.1 == session.id);
                let state = container_mode::control_session_state(session.id);
                json!({
                    "id": session.id.is_saved().then_some(session.id),
                    "migration_required": !session.id.is_saved(),
                    "index": index,
                    "name": session.name,
                    "runtime": session.runtime,
                    "image": session.image,
                    "command": session.command,
                    "profile": session.profile,
                    "display": session.display.as_deref().unwrap_or("auto"),
                    "audio": session.audio,
                    "state": if !session.id.is_saved() { "Migration required" } else { state.as_ref().map(|state| state.0.as_str()).unwrap_or(if tracked.is_some() { "Running" } else { "Idle" }) },
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
        "cursor_paths": container_mode::control_cursor_paths(),
        "performance": container_mode::control_display_performance()
            .into_iter()
            .map(|(slot, redraw_fps, commits_per_second, late_redraws_per_second, max_redraw_wait_ms, input_to_present_ms, sampled_at_unix_ms)| json!({
                "slot": slot,
                "redraw_fps": redraw_fps,
                "commits_per_second": commits_per_second,
                "late_redraws_per_second": late_redraws_per_second,
                "max_redraw_wait_ms": max_redraw_wait_ms,
                "host_input_to_present_ms": input_to_present_ms,
                "host_input_to_submit_ms": input_to_present_ms,
                "latency_measurement": "input-to-CPU-submit estimate; present_ms is a legacy alias, not input-to-photon",
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
    let sessions = inspect_profile_config().sessions;
    Value::Array(
        container_mode::control_active_sessions()
            .into_iter()
            .map(
                |(
                    instance_id,
                    profile_id,
                    started_at_unix_ms,
                    container_pid,
                    waypipe_pid,
                    display_slot,
                    display_pid,
                )| {
                    json!({
                        "instance_id": instance_id,
                        "profile_id": profile_id,
                        "profile_index": sessions.iter().position(|session| session.id == profile_id),
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
    let orbstack_status = command_probe("orbctl", &["status"], &child_path);
    let orbstack_machines = orbstack_machines_snapshot(&orbstack_status, || {
        command_snapshot("orbctl", &["list", "--format", "json"], &child_path)
    });
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
            "system": command_probe("container", &["system", "status", "--format", "json"], &child_path),
        },
        "docker_compatible": {
            "cli": command_probe("docker", &["--version"], &child_path),
            "connection": command_probe("docker", &["info", "--format", "{{.ServerVersion}}"], &child_path),
            "contexts": command_snapshot("docker", &["context", "ls", "--format", "{{json .}}"], &child_path),
        },
        "orbstack_provider": {
            "cli": command_probe("orbctl", &["version"], &child_path),
            "status": orbstack_status,
            "machines": orbstack_machines,
        },
    })
}

fn orbstack_machines_snapshot(status: &Value, list_machines: impl FnOnce() -> Value) -> Value {
    let running = status["result"]["success"].as_bool() == Some(true)
        && status["result"]["stdout"]
            .as_array()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .is_some_and(|stdout| stdout.trim().eq_ignore_ascii_case("running"));
    // `orbctl list` starts a stopped OrbStack, unlike the safe status probe.
    if running {
        list_machines()
    } else {
        json!({
            "available": status["available"],
            "success": false,
            "skipped": true,
            "reason": "OrbStack is stopped or its running state is unconfirmed; machine listing was skipped to avoid starting it.",
        })
    }
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
            "apple_container_system": command_probe("container", &["system", "status", "--format", "json"], &child_path),
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
            "clipboard_image": { "status": "supported", "scope": "bidirectional image/png", "max_encoded_bytes": 67108864 },
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
            "mcp": { "status": "read_only_by_default", "optional_control_flag": "--allow-control", "control": ["open-panel", "check", "launch", "stop", "display-create", "display-close"], "destructive_tools": false },
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
            "id": session.id,
                    "index": index,
            "name": session.name,
            "lines": container_mode::control_session_logs(session.id, limit.clamp(1, 1000)),
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
    crate::process_output::redaction::json(&mut value);
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
        Ok(output) => command_output_snapshot(&output),
        Err(error) => json!({ "available": true, "success": false, "error": error }),
    }
}

fn command_output_snapshot(output: &Output) -> Value {
    let mut data = serde_json::from_slice::<Value>(&output.stdout).ok();
    if let Some(data) = &mut data {
        crate::process_output::redaction::json(data);
    }
    let stdout = crate::process_output::redaction::output(&output.stdout);
    let stderr = crate::process_output::redaction::output(&output.stderr);
    json!({
        "available": true,
        "success": output.status.success(),
        "data": data,
        "stdout": stdout.lines().collect::<Vec<_>>(),
        "stderr": stderr.lines().collect::<Vec<_>>(),
    })
}

fn run_command(
    path: &Path,
    args: &[&str],
    child_path: &str,
    timeout: Duration,
) -> Result<Output, String> {
    let timeout = REQUEST_DEADLINE.with(|budget| match budget.get() {
        Some(deadline) => deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(timeout))
            .ok_or_else(|| "control request deadline exceeded".to_string()),
        None => Ok(timeout),
    })?;
    let mut command = Command::new(path);
    command.env("PATH", child_path).args(args);
    crate::runtime_commands::capture(command, timeout).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_profile(name: &str) -> ContainerSession {
        let mut session: ContainerSession =
            toml::from_str(&format!("name='{name}'\nimage='test'\n")).unwrap();
        session.id = container_sessions::ProfileId::new().unwrap();
        session
    }

    #[test]
    fn legacy_selectors_are_resolved_once_to_ids_before_queuing() {
        let alpha = identity_profile("Alpha");
        let beta = identity_profile("Beta");
        let mut sessions = vec![alpha.clone(), beta.clone()];
        for selector in ["1".to_string(), "beta".to_string(), beta.id.to_string()] {
            let (_, captured) = resolve_session_in(Some(&selector), &sessions).unwrap();
            for command in ["start", "stop", "check"] {
                let message = session_command_message(command, &captured);
                let id = match message {
                    CompositorMessage::StartContainerSessionIfCurrent {
                        profile_id,
                        signature,
                    } => {
                        let target = container_sessions::ProfileActionTarget {
                            id: profile_id,
                            signature,
                        };
                        assert_eq!(target.resolve(&sessions).unwrap().id, beta.id);
                        profile_id
                    }
                    CompositorMessage::StopContainerSession(id)
                    | CompositorMessage::CheckContainerSession(id) => id,
                    _ => panic!("unexpected queued profile action"),
                };
                assert_eq!(id, beta.id);
            }
        }
        let (_, captured) = resolve_session_in(Some("0"), &sessions).unwrap();
        let target = container_sessions::ProfileActionTarget::capture(&captured);
        sessions.remove(0);
        assert_eq!(sessions[0].id, beta.id);
        assert!(target.resolve(&sessions).is_none());
        assert!(resolve_session_in(Some(&alpha.id.to_string()), &sessions).is_err());
        assert_eq!(
            resolve_session_in(Some("0"), &sessions).unwrap().1.id,
            beta.id
        );
    }

    #[test]
    fn ambiguous_names_require_a_persistent_id() {
        let first = identity_profile("Same");
        let second = identity_profile("same");
        let sessions = [first.clone(), second];
        assert!(resolve_session_in(Some("SAME"), &sessions).is_err());
        assert_eq!(
            resolve_session_in(Some(&first.id.to_string()), &sessions)
                .unwrap()
                .1
                .id,
            first.id
        );
    }

    #[test]
    fn stop_by_persistent_id_does_not_consult_broken_configuration() {
        let id = container_sessions::ProfileId::new().unwrap();
        let captured = stop_profile_id(Some(&format!(" {id} ")), |_| {
            panic!("stopping an owned ID must not read configuration")
        })
        .unwrap();
        assert_eq!(captured, id);
        let path = container_sessions::config_path();
        let broken = "[[session]\nthis is not TOML";
        std::fs::write(&path, broken).unwrap();
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        let response = queue_session_command("stop", Some(&id.to_string()), &sender);
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.data["id"], id.to_string());
        assert!(
            matches!(receiver.try_recv(), Ok(CompositorMessage::StopContainerSession(queued)) if queued == id)
        );
        sender.message_received();
        assert!(!sender.finish_turn());
        assert_eq!(std::fs::read_to_string(path).unwrap(), broken);
        assert!(
            stop_profile_id(Some("legacy name"), |_| Err("invalid configuration".into())).is_err()
        );
    }

    #[test]
    fn read_only_queries_never_create_or_migrate_configuration() {
        let path = container_sessions::config_path();
        // The test harness can reuse a worker thread and its private config.
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        let entries = || {
            let mut names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let before = entries();
        assert!(!path.exists());
        assert_eq!(sessions_snapshot(), json!([]));
        assert_eq!(
            status_snapshot(Path::new("unused"))["configured_sessions"],
            0
        );
        active_sessions_json();
        debug_snapshot(Path::new("unused"));
        performance_snapshot(Path::new("unused"));
        assert!(!path.exists());
        assert_eq!(entries(), before);

        let legacy = "[[session]]\nname='legacy'\nimage='test'\n";
        std::fs::write(&path, legacy).unwrap();
        let before = entries();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let sessions = sessions_snapshot();
        assert!(sessions[0]["id"].is_null());
        assert_eq!(sessions[0]["migration_required"], true);
        assert_eq!(
            status_snapshot(Path::new("unused"))["configuration_migration_required"],
            true
        );
        assert!(
            resolve_session(Some("legacy"))
                .unwrap_err()
                .contains("persistent ID")
        );
        active_sessions_json();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(entries(), before);

        std::fs::write(&path, "[invalid").unwrap();
        assert!(status_snapshot(Path::new("unused"))["configuration_error"].is_string());
        assert!(sessions_snapshot()["error"].is_string());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "[invalid");
    }

    #[test]
    fn panel_requests_queue_only_allowlisted_nonmodal_pages() {
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        for page in ["applications", "displays", "diagnostics"] {
            let response = queue_panel_command(Some(page), &sender);
            assert!(response.ok);
            assert_eq!(response.data["accepted"], true);
            assert_eq!(response.data["page"], page);
            assert!(matches!(
                receiver.try_recv(),
                Ok(CompositorMessage::OpenPanel(_))
            ));
            sender.message_received();
            assert!(!sender.finish_turn());
        }
        for page in ["settings", "shell", "../Applications", ""] {
            assert!(!queue_panel_command(Some(page), &sender).ok);
            assert!(receiver.try_recv().is_err());
        }
    }

    #[test]
    fn performance_debug_snapshots_describe_evidence_limits_without_clipboard_contents() {
        let performance = performance_snapshot(Path::new("unused"));
        assert!(
            performance["measurement_limits"]["input_latency"]
                .as_str()
                .unwrap()
                .contains("not screen presentation")
        );
        assert!(
            performance["measurement_limits"]["percentiles"]
                .as_str()
                .unwrap()
                .contains("does not establish")
        );
        let snapshot = debug_snapshot(Path::new("unused"));
        assert!(
            snapshot["collection"]
                .as_str()
                .unwrap()
                .contains("no runtime subprocesses")
        );
        assert!(snapshot["status"]["clipboard"].get("contents").is_none());
        assert!(snapshot["status"]["clipboard"].get("text").is_none());
    }

    #[test]
    fn diagnostic_collection_reserves_time_for_the_response() {
        let now = Instant::now();
        assert_eq!(
            collection_deadline(now + TOTAL_TIMEOUT, now),
            now + TOTAL_TIMEOUT - WRITE_TIMEOUT
        );
        assert_eq!(
            collection_deadline(now + Duration::from_millis(400), now),
            now + Duration::from_millis(300)
        );
        assert_eq!(collection_deadline(now, now), now);
    }

    #[test]
    fn exhausted_diagnostic_collection_returns_its_partial_report() {
        let (server, mut peer) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            handle_connection_with(
                server,
                &test_sender(),
                Path::new("unused"),
                Instant::now() + Duration::from_secs(3),
                |request, _, _| {
                    // Exhaust the probe budget directly. Sleeping to its wall
                    // deadline can also exhaust the response reserve on a busy CI runner.
                    REQUEST_DEADLINE.with(|budget| budget.set(Some(Instant::now())));
                    let error = run_command(
                        Path::new("/must-not-execute"),
                        &[],
                        "",
                        Duration::from_secs(3),
                    )
                    .unwrap_err();
                    ControlResponse::success(
                        &request.command,
                        json!({ "partial": true, "probe_error": error }),
                    )
                },
            );
        });
        peer.write_all(b"{\"command\":\"diagnostics\"}\n").unwrap();
        let response = read_frame_before(
            &mut peer,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        let response: ControlResponse = serde_json::from_slice(&response).unwrap();
        assert!(response.ok);
        assert_eq!(response.command, "diagnostics");
        assert_eq!(response.data["partial"], true);
        assert!(
            response.data["probe_error"]
                .as_str()
                .unwrap()
                .contains("deadline exceeded")
        );
        worker.join().unwrap();
    }
    use std::io::{BufRead, BufReader, Write};

    fn test_sender() -> Sender<CompositorMessage> {
        let (sender, _receiver) = std::sync::mpsc::channel();
        Sender::with_waker(sender, || {})
    }

    #[test]
    fn handler_limit_rejects_peers_and_recovers_capacity() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut idle = Vec::new();
        for _ in 0..MAX_ACTIVE_HANDLERS {
            let (server, peer) = UnixStream::pair().unwrap();
            spawn_connection(server, test_sender(), PathBuf::from("unused"), &active).unwrap();
            idle.push(peer);
        }
        assert_eq!(active.load(Ordering::Acquire), MAX_ACTIVE_HANDLERS);
        let (server, mut rejected) = UnixStream::pair().unwrap();
        assert_eq!(
            spawn_connection(server, test_sender(), PathBuf::from("unused"), &active)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        let error = read_frame_before(
            &mut rejected,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        drop(idle);
        let deadline = Instant::now() + Duration::from_secs(3);
        while active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(active.load(Ordering::Acquire), 0);
        let (server, mut peer) = UnixStream::pair().unwrap();
        spawn_connection(server, test_sender(), PathBuf::from("unused"), &active).unwrap();
        peer.write_all(b"{\"command\":\"unsupported-test\"}\n")
            .unwrap();
        let response = read_frame_before(
            &mut peer,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("unsupported command")
        );
        while active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn idle_handler_obeys_total_deadline() {
        let (server, mut peer) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            handle_connection(
                server,
                &test_sender(),
                Path::new("unused"),
                Instant::now() + Duration::from_millis(50),
            )
        });
        let response = read_frame_before(
            &mut peer,
            MAX_RESPONSE_BYTES,
            Instant::now() + Duration::from_secs(1),
        );
        match response {
            Ok(frame) => {
                let response: ControlResponse = serde_json::from_slice(&frame).unwrap();
                assert!(!response.ok);
                assert!(
                    response
                        .error
                        .unwrap()
                        .contains("failed to read control request")
                );
            }
            // If the scheduler exhausts even the reply reserve, the strict
            // total deadline must close the idle connection instead of extending it.
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof),
        }
        worker.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn dropping_an_unstarted_handler_releases_its_permit() {
        let active = Arc::new(AtomicUsize::new(0));
        let permit = HandlerPermit::acquire(&active).unwrap();
        assert_eq!(active.load(Ordering::Acquire), 1);
        drop(permit);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn expired_inventory_budget_does_not_spawn_a_command() {
        REQUEST_DEADLINE.with(|budget| budget.set(Some(Instant::now())));
        let result = run_command(
            Path::new("/must-not-execute"),
            &[],
            "",
            Duration::from_secs(3),
        );
        REQUEST_DEADLINE.with(|budget| budget.set(None));
        assert!(result.unwrap_err().contains("deadline exceeded"));
    }

    #[test]
    fn expired_queue_deadline_rejects_session_and_display_commands_without_enqueueing() {
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        let session = identity_profile("queue-deadline");
        REQUEST_DEADLINE.with(|budget| budget.set(Some(Instant::now())));
        let display = queue_display_command("display-create", Some("extra"), &sender);
        let session = queue_message(&sender, session_command_message("stop", &session));
        REQUEST_DEADLINE.with(|budget| budget.set(None));
        assert!(!display.ok);
        assert!(display.error.unwrap().contains("deadline exceeded"));
        assert_eq!(session, Err("control request deadline exceeded"));
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(!sender.finish_turn());
    }

    #[test]
    fn saturated_queue_honors_remaining_request_budget_for_both_display_commands_and_session() {
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        for _ in 0..crate::event_loop_signal::MAX_PENDING_WORKER_MESSAGES {
            assert!(sender.send(CompositorMessage::ReloadMenu).is_ok());
        }
        for command in ["display-create", "display-close"] {
            let started = Instant::now();
            REQUEST_DEADLINE.with(|budget| budget.set(Some(started + Duration::from_millis(20))));
            let response = queue_display_command(command, Some("extra"), &sender);
            REQUEST_DEADLINE.with(|budget| budget.set(None));
            assert!(!response.ok);
            assert!(response.error.unwrap().contains("not accepted"));
            assert!(started.elapsed() < Duration::from_secs(1));
        }
        let session = identity_profile("queue-saturation");
        let started = Instant::now();
        REQUEST_DEADLINE.with(|budget| budget.set(Some(started + Duration::from_millis(20))));
        let result = queue_message(&sender, session_command_message("stop", &session));
        REQUEST_DEADLINE.with(|budget| budget.set(None));
        assert!(result.unwrap_err().contains("not accepted"));
        assert!(started.elapsed() < Duration::from_secs(1));
        for _ in 0..crate::event_loop_signal::MAX_PENDING_WORKER_MESSAGES {
            assert!(matches!(
                receiver.try_recv(),
                Ok(CompositorMessage::ReloadMenu)
            ));
            sender.message_received();
        }
        assert!(!sender.finish_turn());
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn saturated_control_admission_releases_all_handler_permits_with_a_short_default_budget() {
        let (sender, _receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        for _ in 0..crate::event_loop_signal::MAX_PENDING_WORKER_MESSAGES {
            assert!(sender.send(CompositorMessage::ReloadMenu).is_ok());
        }
        let active = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        let started = Instant::now();
        for _ in 0..MAX_ACTIVE_HANDLERS {
            let permit = HandlerPermit::acquire(&active).unwrap();
            let sender = sender.clone();
            workers.push(std::thread::spawn(move || {
                let _permit = permit;
                let result = queue_message(&sender, CompositorMessage::CreateManagedDisplay(None));
                assert!(result.unwrap_err().contains("not accepted"));
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert!(HandlerPermit::acquire(&active).is_some());
    }

    #[test]
    fn structured_command_data_is_preserved_even_when_the_service_is_stopped() {
        use std::os::unix::process::ExitStatusExt;
        let output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: br#"{"status":"not running","paths":{"appRoot":"/tmp/container"},"Env":["ORDINARY=hidden-env"]}"#.to_vec(),
            stderr: Vec::new(),
        };
        let snapshot = command_output_snapshot(&output);
        assert_eq!(snapshot["success"], false);
        assert_eq!(snapshot["data"]["status"], "not running");
        assert_eq!(snapshot["data"]["paths"]["appRoot"], "/tmp/container");
        assert_eq!(snapshot["data"]["Env"], "[REDACTED]");
        let rendered = snapshot["stdout"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            serde_json::from_str::<Value>(&rendered).unwrap(),
            snapshot["data"]
        );
        assert!(!rendered.contains("hidden-env"));
        // Sanitizing a diagnostic view must not rewrite operational capture.
        assert!(String::from_utf8_lossy(&output.stdout).contains("hidden-env"));
    }

    #[test]
    fn orbstack_machine_listing_is_skipped_unless_running_is_confirmed() {
        for status in [
            json!({ "available": false, "error": "orbctl command not found" }),
            json!({ "available": true, "result": { "success": false, "error": "timeout" } }),
            json!({ "available": true, "result": { "success": false, "stdout": ["Running"] } }),
            json!({ "available": true, "result": { "success": true, "stdout": ["Stopped"] } }),
            json!({ "available": true, "result": { "success": true, "stdout": ["OrbStack is not running"] } }),
            json!({ "available": true, "result": { "success": true, "stdout": ["Unknown"] } }),
            json!({ "available": true, "result": { "success": true, "stdout": ["Checking whether OrbStack is running"] } }),
            json!({ "available": true, "result": { "success": true, "stdout": [] } }),
            json!({ "available": true, "result": { "success": true } }),
        ] {
            let snapshot = orbstack_machines_snapshot(&status, || {
                panic!("machine listing must not run for status {status}")
            });
            assert_eq!(snapshot["available"], status["available"]);
            assert_eq!(snapshot["success"], false);
            assert_eq!(snapshot["skipped"], true);
            assert!(
                snapshot["reason"]
                    .as_str()
                    .unwrap()
                    .contains("avoid starting")
            );
        }
    }

    #[test]
    fn orbstack_machine_listing_preserves_inventory_when_running() {
        let inventory = json!({
            "available": true,
            "success": true,
            "data": [{ "name": "ubuntu" }],
            "stdout": ["[{\"name\":\"ubuntu\"}]"],
            "stderr": [],
        });
        for stdout in [
            json!(["Running"]),
            json!(["  RUNNING  "]),
            json!(["", "running", ""]),
        ] {
            let status = json!({
                "available": true,
                "result": { "success": true, "stdout": stdout },
            });
            let mut listed = false;
            let snapshot = orbstack_machines_snapshot(&status, || {
                listed = true;
                inventory.clone()
            });
            assert!(listed);
            assert_eq!(snapshot, inventory);
        }
    }

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
        let sender = Sender::with_waker(sender, || {});
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
        let sender = Sender::with_waker(sender, || {});
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
        let sender = Sender::with_waker(sender, || {});
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
    fn feature_matrix_documents_explicit_control_opt_in_without_resource_deletes() {
        let features = feature_matrix_snapshot();
        assert_eq!(
            features["automation"]["mcp"]["status"],
            "read_only_by_default"
        );
        assert_eq!(
            features["automation"]["mcp"]["optional_control_flag"],
            "--allow-control"
        );
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
    fn feature_matrix_reports_bounded_bidirectional_image_clipboard() {
        let features = feature_matrix_snapshot();
        assert_eq!(
            features["transport"]["clipboard_image"]["scope"],
            "bidirectional image/png"
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
        // macOS Unix socket paths must fit sun_path even when TMPDIR is long.
        let directory = tempfile::Builder::new()
            .prefix("cw-ctl-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = directory.path().join("control.sock");
        let (sender, _receiver) = std::sync::mpsc::channel();
        let sender = Sender::with_waker(sender, || {});
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
