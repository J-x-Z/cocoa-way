use log::info;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::SERIAL_COUNTER;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use winit::event::{ElementState, Event, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
mod application_model;
mod audio;
mod connections;
mod container_mode;
mod container_sessions;
mod control_api;
mod control_protocol;
mod diagnostics;
mod keymap;
mod layout;
mod macos_gestures;
mod menu_bar;
mod messages;
mod metal_renderer;
mod network;
mod presentation;
mod render;
mod runtime_paths;
mod state;

use crate::state::AppState;
use messages::CompositorMessage;

struct ActiveContainerSession {
    instance_id: u64,
    index: usize,
    started_at_unix_ms: u128,
    container_child: Option<std::process::Child>,
    waypipe_child: std::process::Child,
    audio_worker: Option<audio::AudioWorker>,
    display_slot: String,
    display_worker: Option<DisplayWorker>,
    stopping_since: Option<std::time::Instant>,
    force_stop_offered: bool,
}

struct DisplayWorker {
    child: std::process::Child,
    runtime_dir: std::path::PathBuf,
}

struct ManagedDisplay {
    slot: String,
    runtime_dir: String,
    display: String,
    worker: DisplayWorker,
}

struct ActiveClassicConnection {
    name: String,
    child: std::process::Child,
}

fn start_classic_connection(
    connection: &connections::Connection,
    runtime_dir: &str,
    display: &str,
    active: &mut Vec<ActiveClassicConnection>,
) -> Result<(), String> {
    let child = connections::spawn_waypipe(connection, runtime_dir, display)?;
    log::info!(
        "Classic connection '{}' started through waypipe with pid {}",
        connection.name,
        child.id()
    );
    active.push(ActiveClassicConnection {
        name: connection.name.clone(),
        child,
    });
    Ok(())
}

fn reap_classic_connections(active: &mut Vec<ActiveClassicConnection>) {
    active.retain_mut(|connection| match connection.child.try_wait() {
        Ok(Some(status)) => {
            log::info!(
                "Classic connection '{}' exited with {}",
                connection.name,
                status
            );
            false
        }
        Ok(None) => true,
        Err(error) => {
            log::warn!(
                "Could not inspect classic connection '{}': {}",
                connection.name,
                error
            );
            false
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DisplayAssignment {
    Default,
    Dedicated(String),
}

const DISPLAY_WORKER_SLOT_ENV: &str = "COCOA_WAY_DISPLAY_WORKER";
const DISPLAY_WORKER_RUNTIME_ENV: &str = "COCOA_WAY_DISPLAY_RUNTIME_DIR";
const DISPLAY_WORKER_READY_ENV: &str = "COCOA_WAY_DISPLAY_READY_FILE";
const DISPLAY_WORKER_PARENT_ENV: &str = "COCOA_WAY_DISPLAY_PARENT_PID";
const DISPLAY_WORKER_PANIC_LOG: &str = "worker-panic.log";
const DISPLAY_WORKER_PID_FILE: &str = "display.worker";
static DISPLAY_WORKER_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static APPLICATION_INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandLineRequest {
    Help,
    Version,
}

fn command_line_request(arguments: &[String]) -> Option<CommandLineRequest> {
    match arguments {
        [argument] if matches!(argument.as_str(), "-h" | "--help") => {
            Some(CommandLineRequest::Help)
        }
        [argument] if matches!(argument.as_str(), "-V" | "--version") => {
            Some(CommandLineRequest::Version)
        }
        _ => None,
    }
}

fn print_command_line_request(request: CommandLineRequest) {
    match request {
        CommandLineRequest::Help => println!(
            "cocoa-way {}\n\nUsage: cocoa-way\n       cocoa-way --help\n       cocoa-way --version\n\nThe GUI exposes the local Wayland compositor, saved connections, and container control panel.",
            env!("CARGO_PKG_VERSION")
        ),
        CommandLineRequest::Version => println!("cocoa-way {}", env!("CARGO_PKG_VERSION")),
    }
}

fn display_slot_slug(value: &str) -> String {
    let slug = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "display".into()
    } else {
        slug
    }
}

fn normalize_managed_display_slot(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("display name cannot be empty".into());
    }
    if value.chars().count() > 64 {
        return Err("display name cannot be longer than 64 characters".into());
    }
    let slot = display_slot_slug(value);
    if matches!(slot.as_str(), "auto" | "default" | "dedicated") {
        return Err(format!(
            "'{slot}' is a display policy keyword; choose a distinct managed display name"
        ));
    }
    Ok(slot)
}

fn choose_display_assignment(
    session: &container_sessions::ContainerSession,
    default_in_use: bool,
) -> DisplayAssignment {
    let requested = session.display.as_deref().map(str::trim).unwrap_or("auto");
    if session.presentation_mode().is_rootless() {
        let requested = match requested {
            "" | "auto" | "default" | "dedicated" => session.name.as_str(),
            named => named,
        };
        return DisplayAssignment::Dedicated(format!("rootless-{}", display_slot_slug(requested)));
    }
    match requested {
        "" | "auto" if !default_in_use => DisplayAssignment::Default,
        "" | "auto" | "dedicated" => {
            DisplayAssignment::Dedicated(format!("session-{}", display_slot_slug(&session.name)))
        }
        "default" => DisplayAssignment::Default,
        named => DisplayAssignment::Dedicated(display_slot_slug(named)),
    }
}

fn active_display_conflict_index(
    requested_index: usize,
    sessions: &[container_sessions::ContainerSession],
    active_slots: impl IntoIterator<Item = (usize, String)>,
) -> Option<usize> {
    let requested = sessions.get(requested_index)?;
    let active_slots = active_slots.into_iter().collect::<Vec<_>>();
    let default_in_use = active_slots
        .iter()
        .any(|(active_index, slot)| *active_index != requested_index && slot == "default");
    let requested_slot = match choose_display_assignment(requested, default_in_use) {
        DisplayAssignment::Default => "default".to_string(),
        DisplayAssignment::Dedicated(slot) => slot,
    };
    active_slots
        .into_iter()
        .find(|(active_index, slot)| *active_index != requested_index && slot == &requested_slot)
        .map(|(active_index, _)| active_index)
}

fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn display_runtime_parent_pid(runtime_dir: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(runtime_dir.join("display.parent"))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .or_else(|| {
            let name = runtime_dir.file_name()?.to_str()?;
            let remainder = name.strip_prefix("cwd-")?;
            remainder.split('-').next()?.parse().ok()
        })
}

fn cleanup_stale_display_runtime_dirs() {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("cwd-") || !path.join("display.slot").is_file() {
            continue;
        }
        let Some(parent_pid) = display_runtime_parent_pid(&path) else {
            continue;
        };
        if parent_pid == std::process::id() || process_exists(parent_pid) {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => log::info!("Removed stale display runtime {}", path.display()),
            Err(error) => log::warn!(
                "Could not remove stale display runtime {}: {}",
                path.display(),
                error
            ),
        }
    }
}

fn display_worker_panic_detail(runtime_dir: &std::path::Path) -> Option<String> {
    let detail = std::fs::read_to_string(runtime_dir.join(DISPLAY_WORKER_PANIC_LOG)).ok()?;
    let detail = detail.trim();
    if detail.is_empty() {
        None
    } else {
        Some(
            detail
                .lines()
                .take(12)
                .collect::<Vec<_>>()
                .join(" | ")
                .chars()
                .take(1_500)
                .collect(),
        )
    }
}

fn install_display_worker_panic_report() {
    let Some(runtime_dir) = std::env::var_os(DISPLAY_WORKER_RUNTIME_ENV) else {
        return;
    };
    let panic_log = std::path::PathBuf::from(runtime_dir).join(DISPLAY_WORKER_PANIC_LOG);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let report = format!(
            "{}\n{}",
            panic_info,
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::write(&panic_log, report);
        previous_hook(panic_info);
    }));
}

fn spawn_display_worker(
    slot: &str,
    presentation: presentation::PresentationMode,
) -> Result<(DisplayWorker, String, String), String> {
    let sequence = DISPLAY_WORKER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let runtime_dir =
        std::path::PathBuf::from(format!("/tmp/cwd-{}-{}", std::process::id(), sequence));
    std::fs::create_dir_all(&runtime_dir)
        .map_err(|error| format!("failed to create display runtime directory: {}", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("failed to secure display runtime directory: {}", error))?;
    }
    if let Err(error) = std::fs::write(runtime_dir.join("display.slot"), slot) {
        let _ = std::fs::remove_dir_all(&runtime_dir);
        return Err(format!(
            "failed to publish dedicated display slot '{}': {}",
            slot, error
        ));
    }
    if let Err(error) = std::fs::write(
        runtime_dir.join("display.parent"),
        std::process::id().to_string(),
    ) {
        let _ = std::fs::remove_dir_all(&runtime_dir);
        return Err(format!(
            "failed to publish dedicated display parent for '{}': {}",
            slot, error
        ));
    }
    let ready_file = runtime_dir.join("display.ready");
    let executable = std::env::current_exe()
        .map_err(|error| format!("failed to locate Cocoa-Way executable: {}", error))?;
    let log_path = runtime_dir.join("display.log");
    let stdout = std::fs::File::create(&log_path)
        .map_err(|error| format!("failed to create display log: {}", error))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| format!("failed to open display log for stderr: {}", error))?;
    let mut child = std::process::Command::new(executable)
        .env(DISPLAY_WORKER_SLOT_ENV, slot)
        .env(presentation::PresentationMode::ENV, presentation.as_str())
        .env(DISPLAY_WORKER_RUNTIME_ENV, &runtime_dir)
        .env(DISPLAY_WORKER_READY_ENV, &ready_file)
        .env(DISPLAY_WORKER_PARENT_ENV, std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        .spawn()
        .map_err(|error| format!("failed to start dedicated display '{}': {}", slot, error))?;
    if let Err(error) = std::fs::write(
        runtime_dir.join(DISPLAY_WORKER_PID_FILE),
        child.id().to_string(),
    ) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&runtime_dir);
        return Err(format!(
            "failed to publish dedicated display worker for '{}': {}",
            slot, error
        ));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if let Ok(contents) = std::fs::read_to_string(&ready_file) {
            let mut lines = contents.lines();
            if let (Some(runtime), Some(display)) = (lines.next(), lines.next()) {
                if !runtime.is_empty() && !display.is_empty() {
                    return Ok((
                        DisplayWorker { child, runtime_dir },
                        runtime.into(),
                        display.into(),
                    ));
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let panic_detail = display_worker_panic_detail(&runtime_dir)
                    .map(|detail| format!("; panic: {}", detail))
                    .unwrap_or_default();
                let _ = std::fs::remove_dir_all(&runtime_dir);
                return Err(format!(
                    "dedicated display '{}' exited before becoming ready: {}{}",
                    slot, status, panic_detail
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&runtime_dir);
                return Err(format!(
                    "failed to monitor dedicated display '{}': {}",
                    slot, error
                ));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&runtime_dir);
    Err(format!(
        "dedicated display '{}' did not publish a Wayland socket within 8 seconds",
        slot
    ))
}

fn spawn_display_worker_async(
    index: usize,
    display_slot: String,
    presentation: presentation::PresentationMode,
    sender: Sender<CompositorMessage>,
) {
    std::thread::spawn(move || {
        let message = match spawn_display_worker(&display_slot, presentation) {
            Ok((worker, runtime_dir, display)) => CompositorMessage::DedicatedDisplayStarted {
                index,
                display_slot,
                runtime_dir,
                display,
                worker_child: worker.child,
                worker_runtime_dir: worker.runtime_dir,
            },
            Err(error) => CompositorMessage::DedicatedDisplayFailed {
                index,
                display_slot,
                error,
            },
        };
        if let Err(send_error) = sender.send(message)
            && let CompositorMessage::DedicatedDisplayStarted {
                worker_child,
                worker_runtime_dir,
                ..
            } = send_error.0
        {
            let mut worker = DisplayWorker {
                child: worker_child,
                runtime_dir: worker_runtime_dir,
            };
            let _ = terminate_display_worker(&mut worker);
        }
    });
}

fn spawn_managed_display_worker_async(display_slot: String, sender: Sender<CompositorMessage>) {
    std::thread::spawn(move || {
        let message =
            match spawn_display_worker(&display_slot, presentation::PresentationMode::Desktop) {
                Ok((worker, runtime_dir, display)) => CompositorMessage::ManagedDisplayStarted {
                    display_slot,
                    runtime_dir,
                    display,
                    worker_child: worker.child,
                    worker_runtime_dir: worker.runtime_dir,
                },
                Err(error) => CompositorMessage::ManagedDisplayFailed {
                    display_slot,
                    error,
                },
            };
        if let Err(send_error) = sender.send(message)
            && let CompositorMessage::ManagedDisplayStarted {
                worker_child,
                worker_runtime_dir,
                ..
            } = send_error.0
        {
            let mut worker = DisplayWorker {
                child: worker_child,
                runtime_dir: worker_runtime_dir,
            };
            let _ = terminate_display_worker(&mut worker);
        }
    });
}

fn terminate_display_worker(worker: &mut DisplayWorker) -> Result<(), String> {
    let result = terminate_child(&mut worker.child);
    let cleanup = std::fs::remove_dir_all(&worker.runtime_dir).or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    });
    result.and(cleanup.map_err(|error| error.to_string()))
}

fn terminate_child(child: &mut std::process::Child) -> Result<(), String> {
    match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            child.kill().map_err(|e| e.to_string())?;
            child.wait().map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn request_child_exit(child: &mut std::process::Child) -> Result<(), String> {
    match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            let result = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            if result == 0 {
                Ok(())
            } else {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    Ok(())
                } else {
                    Err(error.to_string())
                }
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

fn request_graceful_container_stop(
    active: &mut [ActiveContainerSession],
    index: usize,
) -> Result<(), String> {
    let Some(session) = active.iter_mut().find(|session| session.index == index) else {
        return Err("No active instance is tracked for this profile".into());
    };
    if session.stopping_since.is_some() {
        return Ok(());
    }
    if let Some(audio_worker) = session.audio_worker.as_mut() {
        audio_worker.stop();
    }
    request_child_exit(&mut session.waypipe_child)?;
    if let Some(container_child) = session.container_child.as_mut() {
        request_child_exit(container_child)?;
    }
    session.stopping_since = Some(std::time::Instant::now());
    session.force_stop_offered = false;
    Ok(())
}

fn stop_active_container_session(
    active: &mut Vec<ActiveContainerSession>,
    index: usize,
) -> Result<(), String> {
    let Some(position) = active.iter().position(|session| session.index == index) else {
        return Err("No active process is tracked for this session".into());
    };
    let mut session = active.remove(position);
    if let Some(audio_worker) = session.audio_worker.as_mut() {
        audio_worker.stop();
    }
    let waypipe_result = terminate_child(&mut session.waypipe_child);
    let container_result = if let Some(container_child) = session.container_child.as_mut() {
        terminate_child(container_child)
    } else {
        Ok(())
    };
    let display_result = if let Some(display_worker) = session.display_worker.as_mut() {
        terminate_display_worker(display_worker)
    } else {
        Ok(())
    };
    waypipe_result.and(container_result).and(display_result)
}

fn cleanup_named_container_session(index: usize) {
    let Some(session) = container_sessions::load_sessions().get(index).cloned() else {
        return;
    };
    if let Err(error) = container_sessions::cleanup_named_session(&session) {
        log::warn!(
            "Container session #{} named cleanup failed: {}",
            index,
            error
        );
    }
}

fn finish_reaped_container_session(
    index: usize,
    process: &str,
    status: &str,
    requested_stop: bool,
    had_display: bool,
) {
    if requested_stop {
        container_mode::record_stop_progress(
            index,
            "Stop Waypipe worker",
            "Waypipe worker exited and no longer owns the application transport",
        );
        if had_display {
            container_mode::record_stop_progress(
                index,
                "Release display",
                "Dedicated display resources were released",
            );
        }
        container_mode::record_stop_progress(
            index,
            "Stop container",
            "Container process exited and named runtime resources were cleaned up",
        );
        container_mode::record_stop_progress(
            index,
            "Mark instance exited",
            "Application instance is no longer running",
        );
        container_mode::record_stop_success(index);
    } else {
        container_mode::record_process_exit(index, process, status);
    }
}

fn reap_exited_container_sessions(active: &mut Vec<ActiveContainerSession>) {
    let mut position = 0;
    while position < active.len() {
        let index = active[position].index;
        let display_worker_state = active[position]
            .display_worker
            .as_mut()
            .map(|display_worker| display_worker.child.try_wait());
        if let Some(worker_state) = display_worker_state {
            match worker_state {
                Ok(Some(status)) => {
                    let mut session = active.remove(position);
                    let requested_stop = session.stopping_since.is_some();
                    let had_display = session.display_worker.is_some();
                    let _ = terminate_child(&mut session.waypipe_child);
                    if let Some(container_child) = session.container_child.as_mut() {
                        let _ = terminate_child(container_child);
                    }
                    if let Some(display_worker) = session.display_worker.as_ref() {
                        let _ = std::fs::remove_dir_all(&display_worker.runtime_dir);
                    }
                    cleanup_named_container_session(index);
                    finish_reaped_container_session(
                        index,
                        "dedicated display",
                        &status.to_string(),
                        requested_stop,
                        had_display,
                    );
                    continue;
                }
                Err(error) => {
                    let mut session = active.remove(position);
                    let requested_stop = session.stopping_since.is_some();
                    let had_display = session.display_worker.is_some();
                    let _ = terminate_child(&mut session.waypipe_child);
                    if let Some(container_child) = session.container_child.as_mut() {
                        let _ = terminate_child(container_child);
                    }
                    if let Some(display_worker) = session.display_worker.as_mut() {
                        let _ = terminate_display_worker(display_worker);
                    }
                    cleanup_named_container_session(index);
                    finish_reaped_container_session(
                        index,
                        "dedicated display monitor",
                        &format!("error: {}", error),
                        requested_stop,
                        had_display,
                    );
                    continue;
                }
                Ok(None) => {}
            }
        }
        if let Some(container_child) = active[position].container_child.as_mut() {
            match container_child.try_wait() {
                Ok(Some(status)) => {
                    let mut session = active.remove(position);
                    let requested_stop = session.stopping_since.is_some();
                    let had_display = session.display_worker.is_some();
                    let _ = terminate_child(&mut session.waypipe_child);
                    if let Some(display_worker) = session.display_worker.as_mut() {
                        let _ = terminate_display_worker(display_worker);
                    }
                    cleanup_named_container_session(index);
                    finish_reaped_container_session(
                        index,
                        "container",
                        &status.to_string(),
                        requested_stop,
                        had_display,
                    );
                    continue;
                }
                Err(error) => {
                    let mut session = active.remove(position);
                    let requested_stop = session.stopping_since.is_some();
                    let had_display = session.display_worker.is_some();
                    let _ = terminate_child(&mut session.waypipe_child);
                    if let Some(display_worker) = session.display_worker.as_mut() {
                        let _ = terminate_display_worker(display_worker);
                    }
                    cleanup_named_container_session(index);
                    finish_reaped_container_session(
                        index,
                        "container monitor",
                        &format!("error: {}", error),
                        requested_stop,
                        had_display,
                    );
                    continue;
                }
                Ok(None) => {}
            }
        }

        match active[position].waypipe_child.try_wait() {
            Ok(Some(status)) => {
                let mut session = active.remove(position);
                let requested_stop = session.stopping_since.is_some();
                let had_display = session.display_worker.is_some();
                if let Some(container_child) = session.container_child.as_mut() {
                    let _ = terminate_child(container_child);
                }
                if let Some(display_worker) = session.display_worker.as_mut() {
                    let _ = terminate_display_worker(display_worker);
                }
                cleanup_named_container_session(index);
                finish_reaped_container_session(
                    index,
                    "waypipe",
                    &status.to_string(),
                    requested_stop,
                    had_display,
                );
                continue;
            }
            Err(error) => {
                let mut session = active.remove(position);
                let requested_stop = session.stopping_since.is_some();
                let had_display = session.display_worker.is_some();
                if let Some(container_child) = session.container_child.as_mut() {
                    let _ = terminate_child(container_child);
                }
                if let Some(display_worker) = session.display_worker.as_mut() {
                    let _ = terminate_display_worker(display_worker);
                }
                cleanup_named_container_session(index);
                finish_reaped_container_session(
                    index,
                    "waypipe monitor",
                    &format!("error: {}", error),
                    requested_stop,
                    had_display,
                );
                continue;
            }
            Ok(None) => {}
        }

        if active[position]
            .stopping_since
            .is_some_and(|started| started.elapsed() >= std::time::Duration::from_secs(4))
            && !active[position].force_stop_offered
        {
            active[position].force_stop_offered = true;
            container_mode::record_force_stop_available(index);
        }

        position += 1;
    }
}

fn sync_active_container_sessions(active: &[ActiveContainerSession]) {
    container_mode::record_active_container_sessions(
        active
            .iter()
            .map(|session| {
                (
                    session.instance_id,
                    session.index,
                    session.started_at_unix_ms,
                    session.container_child.as_ref().map(|child| child.id()),
                    session.waypipe_child.id(),
                    session.display_slot.clone(),
                    session
                        .display_worker
                        .as_ref()
                        .map(|worker| worker.child.id()),
                    session
                        .display_worker
                        .as_ref()
                        .map(|worker| worker.runtime_dir.to_string_lossy().into_owned()),
                )
            })
            .collect(),
    );
}

fn sync_managed_displays(displays: &[ManagedDisplay]) {
    container_mode::record_managed_displays(
        displays
            .iter()
            .map(|display| {
                (
                    display.slot.clone(),
                    display.runtime_dir.clone(),
                    display.display.clone(),
                    display.worker.child.id(),
                )
            })
            .collect(),
    );
}

fn next_managed_display_slot(
    displays: &[ManagedDisplay],
    pending: &std::collections::HashSet<String>,
    active: &[ActiveContainerSession],
) -> String {
    for number in 1usize.. {
        let candidate = format!("display-{}", number);
        if !displays.iter().any(|display| display.slot == candidate)
            && !pending.contains(&candidate)
            && !active
                .iter()
                .any(|session| session.display_slot == candidate)
        {
            return candidate;
        }
    }
    unreachable!("the display slot counter is unbounded")
}

fn reap_exited_managed_displays(
    displays: &mut Vec<ManagedDisplay>,
    active: &mut Vec<ActiveContainerSession>,
) {
    let mut position = 0;
    while position < displays.len() {
        let state = displays[position].worker.child.try_wait();
        match state {
            Ok(Some(status)) => {
                let display = displays.remove(position);
                let affected = active
                    .iter()
                    .filter(|session| session.display_slot == display.slot)
                    .map(|session| session.index)
                    .collect::<Vec<_>>();
                for index in affected {
                    let _ = stop_active_container_session(active, index);
                    cleanup_named_container_session(index);
                }
                let _ = std::fs::remove_dir_all(&display.worker.runtime_dir);
                container_mode::record_managed_display_exit(&display.slot, &status.to_string());
            }
            Ok(None) => position += 1,
            Err(error) => {
                let mut display = displays.remove(position);
                let _ = terminate_display_worker(&mut display.worker);
                container_mode::record_managed_display_exit(
                    &display.slot,
                    &format!("monitor error: {}", error),
                );
            }
        }
    }
}

fn spawn_output_reader<R>(
    sender: Sender<CompositorMessage>,
    index: usize,
    source: &'static str,
    reader: R,
) where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = sender.send(CompositorMessage::ContainerSessionLog {
                        index,
                        source: source.into(),
                        line,
                    });
                }
                Err(error) => {
                    let _ = sender.send(CompositorMessage::ContainerSessionLog {
                        index,
                        source: source.into(),
                        line: format!("failed to read process output: {}", error),
                    });
                    break;
                }
            }
        }
    });
}

fn attach_container_logs(
    sender: &Sender<CompositorMessage>,
    index: usize,
    report: &mut container_sessions::LaunchReport,
) {
    if let Some(container_child) = report.container_child.as_mut() {
        if let Some(stdout) = container_child.stdout.take() {
            spawn_output_reader(sender.clone(), index, "container stdout", stdout);
        }
        if let Some(stderr) = container_child.stderr.take() {
            spawn_output_reader(sender.clone(), index, "container stderr", stderr);
        }
    }
    if let Some(stdout) = report.waypipe_child.stdout.take() {
        spawn_output_reader(sender.clone(), index, "waypipe stdout", stdout);
    }
    if let Some(stderr) = report.waypipe_child.stderr.take() {
        spawn_output_reader(sender.clone(), index, "waypipe stderr", stderr);
    }
}

fn spawn_container_session_check(
    index: usize,
    session: container_sessions::ContainerSession,
    launch_after: bool,
    sender: Sender<CompositorMessage>,
) {
    std::thread::spawn(move || {
        if launch_after {
            let _ = sender.send(CompositorMessage::ContainerSessionLaunchProgress {
                index,
                step: application_model::LaunchStep::ValidateProfile,
                detail: "Validating the saved application profile".into(),
            });
            let _ = sender.send(CompositorMessage::ContainerSessionLaunchProgress {
                index,
                step: application_model::LaunchStep::CheckRuntime,
                detail: "Checking the selected container runtime and transport".into(),
            });
        }
        let result = container_sessions::check_session(&session);
        let _ = sender.send(CompositorMessage::ContainerSessionChecked {
            index,
            launch_after,
            result,
        });
    });
}

fn spawn_container_session_on_display(
    index: usize,
    session: container_sessions::ContainerSession,
    check: container_sessions::CheckReport,
    runtime_dir: String,
    display: String,
    display_slot: String,
    sender: Sender<CompositorMessage>,
) {
    std::thread::spawn(move || {
        let progress_sender = sender.clone();
        let result = container_sessions::launch_checked_session(
            &session,
            &runtime_dir,
            &display,
            check,
            move |step, detail| {
                let _ = progress_sender.send(CompositorMessage::ContainerSessionLaunchProgress {
                    index,
                    step,
                    detail: detail.into(),
                });
            },
        );
        let _ = sender.send(CompositorMessage::ContainerSessionLaunchFinished {
            index,
            display_slot,
            result,
        });
    });
}

fn complete_container_session_launch(
    index: usize,
    session: &container_sessions::ContainerSession,
    display_slot: String,
    mut display_worker: Option<DisplayWorker>,
    result: Result<container_sessions::LaunchReport, container_sessions::LaunchError>,
    sender: &Sender<CompositorMessage>,
    active: &mut Vec<ActiveContainerSession>,
) {
    match result {
        Ok(mut report) => {
            log::info!(
                "Container session #{} started through {} on {} using display {}",
                index,
                report.runtime,
                report.host_socket,
                display_slot
            );
            container_mode::record_launch_success(index, &report);
            attach_container_logs(sender, index, &mut report);
            let audio_worker = report
                .audio_host_socket
                .take()
                .map(|socket| audio::AudioWorker::start(index, session.name.clone(), socket));
            active.push(ActiveContainerSession {
                instance_id: APPLICATION_INSTANCE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
                index,
                started_at_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or_default(),
                container_child: report.container_child,
                waypipe_child: report.waypipe_child,
                audio_worker,
                display_slot,
                display_worker,
                stopping_since: None,
                force_stop_offered: false,
            });
            sync_active_container_sessions(active);
        }
        Err(error) => {
            if let Some(worker) = display_worker.as_mut() {
                let _ = terminate_display_worker(worker);
            }
            if !error.is_container_already_running() {
                let cleanup_session = session.clone();
                std::thread::spawn(move || {
                    if let Err(cleanup_error) =
                        container_sessions::cleanup_named_session(&cleanup_session)
                    {
                        log::warn!(
                            "Failed to clean up resources after launch error for '{}': {}",
                            cleanup_session.name,
                            cleanup_error
                        );
                    }
                });
            }
            if error.is_apple_container_transport_blocked() {
                log::info!("Container session #{} is blocked: {}", index, error);
            } else if error.is_container_already_running() {
                log::info!("Container session #{} is already running: {}", index, error);
            } else {
                log::error!("Container session #{} failed: {}", index, error);
            }
            container_mode::record_launch_failure(index, &error);
        }
    }
}

fn container_data_root() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
    format!("{}/Library/Application Support/com.apple.container", home)
}

fn available_disk_gib(path: &str) -> Option<f64> {
    let output = std::process::Command::new("/bin/df")
        .args(["-Pk", path])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let available_kib: u64 = stdout
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(available_kib as f64 / 1024.0 / 1024.0)
}

fn warn_low_container_disk_for_image_task(
    sender: &Sender<CompositorMessage>,
    runtime: &str,
    image: &str,
) {
    let root = container_data_root();
    if let Some(free_gib) = available_disk_gib(&root) {
        if free_gib < 8.0 {
            let _ = sender.send(CompositorMessage::ContainerImagePullLog {
                runtime: runtime.into(),
                image: image.into(),
                line: format!(
                    "Low disk space: {:.1}G free at {}. Image operations may fail.",
                    free_gib, root
                ),
            });
        }
    }
}

fn image_pull_command(
    runtime: &str,
    image: String,
    platform: Option<&str>,
    scheme: Option<&str>,
) -> Result<(&'static str, Vec<String>), String> {
    let runtime_key = runtime.trim().to_ascii_lowercase();
    match runtime_key.as_str() {
        "container" | "apple" | "apple container" => {
            let mut args = vec![
                "image".into(),
                "pull".into(),
                "--progress".into(),
                "plain".into(),
            ];
            if let Some(scheme) = scheme {
                args.extend(["--scheme".into(), scheme.into()]);
            }
            if let Some(platform) = platform {
                args.extend(["--platform".into(), platform.into()]);
            }
            args.push(image);
            Ok(("container", args))
        }
        "docker" => {
            let mut args = vec!["pull".into()];
            if let Some(platform) = platform {
                args.extend(["--platform".into(), platform.into()]);
            }
            args.push(image);
            Ok(("docker", args))
        }
        "orb" | "orbstack" => Err(
            "OrbStack image pull uses its Docker-compatible context; select that destination instead."
                .into(),
        ),
        _ => Err("Unsupported runtime. Use `container` or `docker`.".into()),
    }
}

fn spawn_image_pull(
    sender: Sender<CompositorMessage>,
    runtime: String,
    image: String,
    platform: Option<String>,
    scheme: Option<String>,
) {
    std::thread::spawn(move || {
        warn_low_container_disk_for_image_task(&sender, &runtime, &image);
        let child_path = runtime_paths::build_child_path();
        let (command, args) = match image_pull_command(
            &runtime,
            image.clone(),
            platform.as_deref(),
            scheme.as_deref(),
        ) {
            Ok(command) => command,
            Err(status) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime,
                    image,
                    success: false,
                    status,
                });
                return;
            }
        };

        let Some(command_path) = runtime_paths::find_command_path(command, &child_path) else {
            let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                runtime,
                image,
                success: false,
                status: format!("Missing command `{}`.", command),
            });
            return;
        };

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime,
                    image,
                    success: false,
                    status: format!("Failed to start image pull: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_image_pull_reader(sender.clone(), runtime.clone(), image.clone(), stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_image_pull_reader(sender.clone(), runtime.clone(), image.clone(), stderr);
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime,
                    image,
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime,
                    image,
                    success: false,
                    status: format!("Failed to wait for image pull: {}", error),
                });
            }
        }
    });
}

fn spawn_image_pull_reader<R>(
    sender: Sender<CompositorMessage>,
    runtime: String,
    image: String,
    reader: R,
) where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = sender.send(CompositorMessage::ContainerImagePullLog {
                        runtime: runtime.clone(),
                        image: image.clone(),
                        line,
                    });
                }
                Err(error) => {
                    let _ = sender.send(CompositorMessage::ContainerImagePullLog {
                        runtime: runtime.clone(),
                        image: image.clone(),
                        line: format!("failed to read image pull output: {}", error),
                    });
                    break;
                }
            }
        }
    });
}

fn spawn_registry_login(
    sender: Sender<CompositorMessage>,
    server: String,
    username: String,
    password: String,
    scheme: Option<String>,
) {
    std::thread::spawn(move || {
        let action = format!("registry login {}", server);
        let child_path = runtime_paths::build_child_path();
        let Some(command_path) = runtime_paths::find_command_path("container", &child_path) else {
            let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                runtime: "apple".into(),
                action,
                success: false,
                status: "Missing command `container`.".into(),
            });
            return;
        };
        let args = registry_login_args(&server, &username, scheme.as_deref());

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime: "apple".into(),
                    action,
                    success: false,
                    status: format!("Failed to start registry login: {}", error),
                });
                return;
            }
        };

        let write_result = child.stdin.take().map(|mut stdin| {
            stdin.write_all(password.as_bytes())?;
            stdin.write_all(b"\n")
        });
        if let Some(Err(error)) = write_result {
            let _ = child.kill();
            let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                runtime: "apple".into(),
                action,
                success: false,
                status: format!("Failed to send registry credentials: {}", error),
            });
            return;
        }

        match child.wait_with_output() {
            Ok(output) => {
                for line in String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .chain(String::from_utf8_lossy(&output.stderr).lines())
                    .filter(|line| !line.trim().is_empty())
                {
                    let _ = sender.send(CompositorMessage::RuntimeSystemActionLog {
                        runtime: "apple".into(),
                        action: action.clone(),
                        line: line.to_string(),
                    });
                }
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime: "apple".into(),
                    action,
                    success: output.status.success(),
                    status: output.status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime: "apple".into(),
                    action,
                    success: false,
                    status: format!("Failed to wait for registry login: {}", error),
                });
            }
        }
    });
}

fn registry_login_args(server: &str, username: &str, scheme: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "registry".to_string(),
        "login".into(),
        "--password-stdin".into(),
        "--username".into(),
        username.into(),
    ];
    if let Some(scheme) = scheme {
        args.extend(["--scheme".into(), scheme.into()]);
    }
    args.push(server.into());
    args
}

fn spawn_image_load(sender: Sender<CompositorMessage>, path: String) {
    std::thread::spawn(move || {
        warn_low_container_disk_for_image_task(&sender, "load", &path);
        let child_path = runtime_paths::build_child_path();
        let Some(command_path) = runtime_paths::find_command_path("container", &child_path) else {
            let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                runtime: "load".into(),
                image: path,
                success: false,
                status: "Missing command `container`.".into(),
            });
            return;
        };

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(["image", "load", "--input", &path])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "load".into(),
                    image: path,
                    success: false,
                    status: format!("Failed to start image load: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_image_pull_reader(sender.clone(), "load".into(), path.clone(), stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_image_pull_reader(sender.clone(), "load".into(), path.clone(), stderr);
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "load".into(),
                    image: path,
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "load".into(),
                    image: path,
                    success: false,
                    status: format!("Failed to wait for image load: {}", error),
                });
            }
        }
    });
}

fn spawn_image_build(
    sender: Sender<CompositorMessage>,
    image: String,
    containerfile: String,
    context: String,
) {
    std::thread::spawn(move || {
        warn_low_container_disk_for_image_task(&sender, "build", &image);
        let child_path = runtime_paths::build_child_path();
        let Some(command_path) = runtime_paths::find_command_path("container", &child_path) else {
            let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                runtime: "build".into(),
                image,
                success: false,
                status: "Missing command `container`.".into(),
            });
            return;
        };

        let args = [
            "build".to_string(),
            "--progress".to_string(),
            "plain".to_string(),
            "-f".to_string(),
            containerfile,
            "-t".to_string(),
            image.clone(),
            context,
        ];
        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "build".into(),
                    image,
                    success: false,
                    status: format!("Failed to start image build: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_image_pull_reader(sender.clone(), "build".into(), image.clone(), stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_image_pull_reader(sender.clone(), "build".into(), image.clone(), stderr);
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "build".into(),
                    image,
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "build".into(),
                    image,
                    success: false,
                    status: format!("Failed to wait for image build: {}", error),
                });
            }
        }
    });
}

fn spawn_apple_container_system_start(sender: Sender<CompositorMessage>) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let Some(command_path) = runtime_paths::find_command_path("container", &child_path) else {
            let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                runtime: "system".into(),
                image: "Apple Container".into(),
                success: false,
                status: "Missing command `container`.".into(),
            });
            return;
        };

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(["system", "start"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "system".into(),
                    image: "Apple Container".into(),
                    success: false,
                    status: format!("Failed to start Apple Container system: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_image_pull_reader(
                sender.clone(),
                "system".into(),
                "Apple Container".into(),
                stdout,
            );
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_image_pull_reader(
                sender.clone(),
                "system".into(),
                "Apple Container".into(),
                stderr,
            );
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "system".into(),
                    image: "Apple Container".into(),
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: "system".into(),
                    image: "Apple Container".into(),
                    success: false,
                    status: format!("Failed to wait for Apple Container system start: {}", error),
                });
            }
        }
    });
}

fn spawn_image_delete(sender: Sender<CompositorMessage>, runtime: String, image: String) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let runtime_key = runtime.trim().to_ascii_lowercase();
        let operation_runtime = format!("delete:{runtime}");
        let (command, args): (&str, Vec<String>) = match runtime_key.as_str() {
            "container" | "apple" | "apple container" => (
                "container",
                vec!["image".into(), "delete".into(), image.clone()],
            ),
            "docker" => ("docker", vec!["image".into(), "rm".into(), image.clone()]),
            _ => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: operation_runtime,
                    image,
                    success: false,
                    status: "Unsupported runtime. Use `container` or `docker`.".into(),
                });
                return;
            }
        };

        let Some(command_path) = runtime_paths::find_command_path(command, &child_path) else {
            let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                runtime: operation_runtime,
                image,
                success: false,
                status: format!("Missing command `{}`.", command),
            });
            return;
        };

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: operation_runtime,
                    image,
                    success: false,
                    status: format!("Failed to start image delete: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_image_pull_reader(
                sender.clone(),
                operation_runtime.clone(),
                image.clone(),
                stdout,
            );
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_image_pull_reader(
                sender.clone(),
                operation_runtime.clone(),
                image.clone(),
                stderr,
            );
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: operation_runtime,
                    image,
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerImagePullFinished {
                    runtime: operation_runtime,
                    image,
                    success: false,
                    status: format!("Failed to wait for image delete: {}", error),
                });
            }
        }
    });
}

fn spawn_volume_delete(sender: Sender<CompositorMessage>, runtime: String, volume: String) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let runtime_key = runtime.trim().to_ascii_lowercase();
        let (command, args): (&str, Vec<String>) = match runtime_key.as_str() {
            "container" | "apple" | "apple container" => (
                "container",
                vec!["volume".into(), "delete".into(), volume.clone()],
            ),
            "docker" => ("docker", vec!["volume".into(), "rm".into(), volume.clone()]),
            _ => {
                let _ = sender.send(CompositorMessage::ContainerVolumeDeleteFinished {
                    runtime,
                    volume,
                    success: false,
                    status: "Unsupported runtime. Use `container` or `docker`.".into(),
                });
                return;
            }
        };

        let Some(command_path) = runtime_paths::find_command_path(command, &child_path) else {
            let _ = sender.send(CompositorMessage::ContainerVolumeDeleteFinished {
                runtime,
                volume,
                success: false,
                status: format!("Missing command `{}`.", command),
            });
            return;
        };

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerVolumeDeleteFinished {
                    runtime,
                    volume,
                    success: false,
                    status: format!("Failed to start volume delete: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_volume_delete_reader(sender.clone(), runtime.clone(), volume.clone(), stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_volume_delete_reader(sender.clone(), runtime.clone(), volume.clone(), stderr);
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::ContainerVolumeDeleteFinished {
                    runtime,
                    volume,
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerVolumeDeleteFinished {
                    runtime,
                    volume,
                    success: false,
                    status: format!("Failed to wait for volume delete: {}", error),
                });
            }
        }
    });
}

fn spawn_volume_create(sender: Sender<CompositorMessage>, runtime: String, volume: String) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let runtime_key = runtime.trim().to_ascii_lowercase();
        let (command, args): (&str, Vec<String>) = match runtime_key.as_str() {
            "container" | "apple" | "apple container" => (
                "container",
                vec!["volume".into(), "create".into(), volume.clone()],
            ),
            "docker" => (
                "docker",
                vec!["volume".into(), "create".into(), volume.clone()],
            ),
            _ => {
                let _ = sender.send(CompositorMessage::ContainerVolumeCreateFinished {
                    runtime,
                    volume,
                    success: false,
                    status: "Unsupported runtime. Use `container` or `docker`.".into(),
                });
                return;
            }
        };

        let Some(command_path) = runtime_paths::find_command_path(command, &child_path) else {
            let _ = sender.send(CompositorMessage::ContainerVolumeCreateFinished {
                runtime,
                volume,
                success: false,
                status: format!("Missing command `{}`.", command),
            });
            return;
        };

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerVolumeCreateFinished {
                    runtime,
                    volume,
                    success: false,
                    status: format!("Failed to start volume create: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_volume_create_reader(sender.clone(), runtime.clone(), volume.clone(), stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_volume_create_reader(sender.clone(), runtime.clone(), volume.clone(), stderr);
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::ContainerVolumeCreateFinished {
                    runtime,
                    volume,
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::ContainerVolumeCreateFinished {
                    runtime,
                    volume,
                    success: false,
                    status: format!("Failed to wait for volume create: {}", error),
                });
            }
        }
    });
}

fn spawn_volume_create_reader<R>(
    sender: Sender<CompositorMessage>,
    runtime: String,
    volume: String,
    reader: R,
) where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = sender.send(CompositorMessage::ContainerVolumeCreateLog {
                        runtime: runtime.clone(),
                        volume: volume.clone(),
                        line,
                    });
                }
                Err(error) => {
                    let _ = sender.send(CompositorMessage::ContainerVolumeCreateLog {
                        runtime: runtime.clone(),
                        volume: volume.clone(),
                        line: format!("failed to read volume create output: {}", error),
                    });
                    break;
                }
            }
        }
    });
}

fn spawn_volume_delete_reader<R>(
    sender: Sender<CompositorMessage>,
    runtime: String,
    volume: String,
    reader: R,
) where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = sender.send(CompositorMessage::ContainerVolumeDeleteLog {
                        runtime: runtime.clone(),
                        volume: volume.clone(),
                        line,
                    });
                }
                Err(error) => {
                    let _ = sender.send(CompositorMessage::ContainerVolumeDeleteLog {
                        runtime: runtime.clone(),
                        volume: volume.clone(),
                        line: format!("failed to read volume delete output: {}", error),
                    });
                    break;
                }
            }
        }
    });
}

fn spawn_runtime_container_action(
    sender: Sender<CompositorMessage>,
    runtime: String,
    name: String,
    action: String,
) {
    if action == "restart"
        && matches!(
            runtime.trim().to_ascii_lowercase().as_str(),
            "apple" | "container"
        )
    {
        spawn_apple_runtime_container_restart(sender, runtime, name);
        return;
    }
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let Some((command, args)) = runtime_container_command(&runtime, &action, &name) else {
            let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                runtime,
                name,
                action,
                success: false,
                status: "Unsupported runtime action.".into(),
            });
            return;
        };

        let Some(command_path) = runtime_paths::find_command_path(command, &child_path) else {
            let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                runtime,
                name,
                action,
                success: false,
                status: format!("Missing command `{}`.", command),
            });
            return;
        };

        let mut child = match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                    runtime,
                    name,
                    action,
                    success: false,
                    status: format!("Failed to start container action: {}", error),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            spawn_runtime_container_action_reader(
                sender.clone(),
                runtime.clone(),
                name.clone(),
                action.clone(),
                stdout,
            );
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_runtime_container_action_reader(
                sender.clone(),
                runtime.clone(),
                name.clone(),
                action.clone(),
                stderr,
            );
        }

        match child.wait() {
            Ok(status) => {
                let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                    runtime,
                    name,
                    action,
                    success: status.success(),
                    status: status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                    runtime,
                    name,
                    action,
                    success: false,
                    status: format!("Failed to wait for container action: {}", error),
                });
            }
        }
    });
}

fn runtime_container_command(
    runtime: &str,
    action: &str,
    name: &str,
) -> Option<(&'static str, Vec<String>)> {
    let runtime = runtime.trim().to_ascii_lowercase();
    match (runtime.as_str(), action) {
        ("apple" | "container", "start" | "stop") => {
            Some(("container", vec![action.into(), name.into()]))
        }
        ("apple" | "container", "delete") => Some((
            "container",
            vec!["delete".into(), "--force".into(), name.into()],
        )),
        ("docker" | "orb" | "orbstack", "start" | "stop") => {
            Some(("docker", vec![action.into(), name.into()]))
        }
        ("docker" | "orb" | "orbstack", "restart") => {
            Some(("docker", vec!["restart".into(), name.into()]))
        }
        ("docker" | "orb" | "orbstack", "delete") => {
            Some(("docker", vec!["rm".into(), "-f".into(), name.into()]))
        }
        _ => None,
    }
}

fn spawn_apple_runtime_container_restart(
    sender: Sender<CompositorMessage>,
    runtime: String,
    name: String,
) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let Some(command_path) = runtime_paths::find_command_path("container", &child_path) else {
            let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                runtime,
                name,
                action: "restart".into(),
                success: false,
                status: "Missing command `container`.".into(),
            });
            return;
        };
        for phase in ["stop", "start"] {
            let output = std::process::Command::new(&command_path)
                .env("PATH", &child_path)
                .args([phase, &name])
                .output();
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                        runtime,
                        name,
                        action: "restart".into(),
                        success: false,
                        status: format!("Failed to {} Apple container: {}", phase, error),
                    });
                    return;
                }
            };
            for line in String::from_utf8_lossy(&output.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&output.stderr).lines())
                .filter(|line| !line.trim().is_empty())
            {
                let _ = sender.send(CompositorMessage::RuntimeContainerActionLog {
                    runtime: runtime.clone(),
                    name: name.clone(),
                    action: "restart".into(),
                    line: format!("{}: {}", phase, line),
                });
            }
            if !output.status.success() {
                let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
                    runtime,
                    name,
                    action: "restart".into(),
                    success: false,
                    status: format!("container {} failed with {}", phase, output.status),
                });
                return;
            }
        }
        let _ = sender.send(CompositorMessage::RuntimeContainerActionFinished {
            runtime,
            name,
            action: "restart".into(),
            success: true,
            status: "stopped and started".into(),
        });
    });
}

fn spawn_runtime_container_action_reader<R>(
    sender: Sender<CompositorMessage>,
    runtime: String,
    name: String,
    action: String,
    reader: R,
) where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    let _ = sender.send(CompositorMessage::RuntimeContainerActionLog {
                        runtime: runtime.clone(),
                        name: name.clone(),
                        action: action.clone(),
                        line,
                    });
                }
                Err(error) => {
                    let _ = sender.send(CompositorMessage::RuntimeContainerActionLog {
                        runtime: runtime.clone(),
                        name: name.clone(),
                        action: action.clone(),
                        line: format!("failed to read runtime container output: {}", error),
                    });
                    break;
                }
            }
        }
    });
}

fn spawn_runtime_system_action(sender: Sender<CompositorMessage>, runtime: String, action: String) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let runtime_key = runtime.trim().to_ascii_lowercase();
        let (command, args): (&str, Vec<&str>) = match runtime_key.as_str() {
            "apple" | "container" => ("container", vec!["system", action.as_str()]),
            "orb" | "orbstack" => ("orbctl", vec![action.as_str()]),
            _ => {
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime,
                    action,
                    success: false,
                    status: "Unsupported runtime system action.".into(),
                });
                return;
            }
        };
        if !matches!(action.as_str(), "start" | "stop") {
            let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                runtime,
                action,
                success: false,
                status: "Unsupported runtime system action.".into(),
            });
            return;
        }
        let Some(command_path) = runtime_paths::find_command_path(command, &child_path) else {
            let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                runtime,
                action,
                success: false,
                status: format!("Missing command `{}`.", command),
            });
            return;
        };

        let output = std::process::Command::new(&command_path)
            .env("PATH", &child_path)
            .args(args)
            .output();
        match output {
            Ok(output) => {
                for line in String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .chain(String::from_utf8_lossy(&output.stderr).lines())
                    .filter(|line| !line.trim().is_empty())
                {
                    let _ = sender.send(CompositorMessage::RuntimeSystemActionLog {
                        runtime: runtime.clone(),
                        action: action.clone(),
                        line: line.to_string(),
                    });
                }
                let mut success = output.status.success();
                let mut status = output.status.to_string();
                if success && matches!(runtime_key.as_str(), "orb" | "orbstack") {
                    match wait_for_orbstack_state(&command_path, &child_path, action == "start") {
                        Ok(observed) => status = observed,
                        Err(error) => {
                            success = false;
                            status = error;
                        }
                    }
                }
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime,
                    action,
                    success,
                    status,
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime,
                    action,
                    success: false,
                    status: format!("Failed to run runtime action: {}", error),
                });
            }
        }
    });
}

fn spawn_runtime_machine_action(
    sender: Sender<CompositorMessage>,
    runtime: String,
    name: String,
    action: String,
) {
    std::thread::spawn(move || {
        let activity_action = format!("machine {} {}", action, name);
        if !matches!(
            runtime.trim().to_ascii_lowercase().as_str(),
            "orb" | "orbstack"
        ) || !matches!(action.as_str(), "start" | "stop" | "delete")
        {
            let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                runtime,
                action: activity_action,
                success: false,
                status: "Unsupported runtime machine action.".into(),
            });
            return;
        }

        let child_path = runtime_paths::build_child_path();
        let Some(command_path) = runtime_paths::find_command_path("orbctl", &child_path) else {
            let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                runtime,
                action: activity_action,
                success: false,
                status: "Missing command `orbctl`.".into(),
            });
            return;
        };
        let mut args = vec![action.clone()];
        if action == "delete" {
            args.push("--force".into());
        }
        args.push(name);
        match std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(&args)
            .output()
        {
            Ok(output) => {
                for line in String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .chain(String::from_utf8_lossy(&output.stderr).lines())
                    .filter(|line| !line.trim().is_empty())
                {
                    let _ = sender.send(CompositorMessage::RuntimeSystemActionLog {
                        runtime: runtime.clone(),
                        action: activity_action.clone(),
                        line: line.to_string(),
                    });
                }
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime,
                    action: activity_action,
                    success: output.status.success(),
                    status: output.status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime,
                    action: activity_action,
                    success: false,
                    status: format!("Failed to run OrbStack machine action: {}", error),
                });
            }
        }
    });
}

fn wait_for_orbstack_state(
    command_path: &std::path::Path,
    child_path: &str,
    expected_running: bool,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
    let mut last_status = "status unavailable".to_string();
    while std::time::Instant::now() < deadline {
        match std::process::Command::new(command_path)
            .env("PATH", child_path)
            .arg("status")
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                last_status = [stdout.trim(), stderr.trim()]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                if parse_orbstack_running(&last_status) == Some(expected_running) {
                    return Ok(format!(
                        "OrbStack is {}",
                        if expected_running {
                            "running"
                        } else {
                            "stopped"
                        }
                    ));
                }
            }
            Err(error) => last_status = error.to_string(),
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(format!(
        "OrbStack did not reach the expected {} state: {}",
        if expected_running {
            "running"
        } else {
            "stopped"
        },
        last_status
    ))
}

fn parse_orbstack_running(status: &str) -> Option<bool> {
    let status = status.trim().to_ascii_lowercase();
    if status.contains("stopped") || status.contains("not running") {
        Some(false)
    } else if status.contains("running") {
        Some(true)
    } else {
        None
    }
}

fn spawn_docker_context_switch(sender: Sender<CompositorMessage>, name: String) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let action = format!("context use {}", name);
        let Some(command_path) = runtime_paths::find_command_path("docker", &child_path) else {
            let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                runtime: "docker".into(),
                action,
                success: false,
                status: "Missing command `docker`.".into(),
            });
            return;
        };
        let output = std::process::Command::new(command_path)
            .env("PATH", &child_path)
            .args(["context", "use", &name])
            .output();
        match output {
            Ok(output) => {
                for line in String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .chain(String::from_utf8_lossy(&output.stderr).lines())
                    .filter(|line| !line.trim().is_empty())
                {
                    let _ = sender.send(CompositorMessage::RuntimeSystemActionLog {
                        runtime: "docker".into(),
                        action: action.clone(),
                        line: line.to_string(),
                    });
                }
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime: "docker".into(),
                    action,
                    success: output.status.success(),
                    status: output.status.to_string(),
                });
            }
            Err(error) => {
                let _ = sender.send(CompositorMessage::RuntimeSystemActionFinished {
                    runtime: "docker".into(),
                    action,
                    success: false,
                    status: format!("Failed to switch Docker context: {}", error),
                });
            }
        }
    });
}

struct RuntimeDetailCommands {
    command: &'static str,
    info: Vec<String>,
    logs: Vec<String>,
    stats: Vec<String>,
}

fn runtime_detail_commands(runtime: &str, name: &str) -> Option<RuntimeDetailCommands> {
    match runtime.trim().to_ascii_lowercase().as_str() {
        "apple" | "container" => Some(RuntimeDetailCommands {
            command: "container",
            info: vec!["inspect".into(), name.into()],
            logs: vec!["logs".into(), "-n".into(), "20".into(), name.into()],
            stats: vec![
                "stats".into(),
                name.into(),
                "--no-stream".into(),
                "--format".into(),
                "table".into(),
            ],
        }),
        "docker" | "orb" | "orbstack" => Some(RuntimeDetailCommands {
            command: "docker",
            info: vec![
                "inspect".into(),
                "--format".into(),
                "Name: {{.Name}}\nImage: {{.Config.Image}}\nState: {{.State.Status}}\nCreated: {{.Created}}".into(),
                name.into(),
            ],
            logs: vec!["logs".into(), "--tail".into(), "20".into(), name.into()],
            stats: vec![
                "stats".into(),
                "--no-stream".into(),
                "--format".into(),
                "table {{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.NetIO}}\t{{.BlockIO}}".into(),
                name.into(),
            ],
        }),
        _ => None,
    }
}

fn spawn_runtime_container_details(
    sender: Sender<CompositorMessage>,
    runtime: String,
    name: String,
) {
    std::thread::spawn(move || {
        let child_path = runtime_paths::build_child_path();
        let Some(commands) = runtime_detail_commands(&runtime, &name) else {
            let _ = sender.send(CompositorMessage::RuntimeContainerDetailsLoaded {
                runtime,
                name,
                info: Vec::new(),
                logs: Vec::new(),
                stats: Vec::new(),
                error: Some("Unsupported container runtime.".into()),
            });
            return;
        };
        let Some(command_path) = runtime_paths::find_command_path(commands.command, &child_path)
        else {
            let _ = sender.send(CompositorMessage::RuntimeContainerDetailsLoaded {
                runtime,
                name,
                info: Vec::new(),
                logs: Vec::new(),
                stats: Vec::new(),
                error: Some(format!("Missing command `{}`.", commands.command)),
            });
            return;
        };

        let (info, info_error) = runtime_detail_output(
            &command_path,
            &child_path,
            &commands.info,
            std::time::Duration::from_secs(3),
            8,
        );
        let (logs, logs_error) = runtime_detail_output(
            &command_path,
            &child_path,
            &commands.logs,
            std::time::Duration::from_secs(3),
            20,
        );
        let (stats, stats_error) = runtime_detail_output(
            &command_path,
            &child_path,
            &commands.stats,
            std::time::Duration::from_secs(3),
            5,
        );
        let errors = [info_error, logs_error, stats_error]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let _ = sender.send(CompositorMessage::RuntimeContainerDetailsLoaded {
            runtime,
            name,
            info,
            logs,
            stats,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        });
    });
}

fn runtime_detail_output(
    command: &std::path::Path,
    child_path: &str,
    args: &[String],
    timeout: std::time::Duration,
    max_lines: usize,
) -> (Vec<String>, Option<String>) {
    let mut child = match std::process::Command::new(command)
        .env("PATH", child_path)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return (Vec::new(), Some(error.to_string())),
    };
    let deadline = std::time::Instant::now() + timeout;
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output().map_err(|error| error.to_string()),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return (
                    Vec::new(),
                    Some(format!("command timed out after {}ms", timeout.as_millis())),
                );
            }
            Err(error) => return (Vec::new(), Some(error.to_string())),
        }
    };
    let output = match output {
        Ok(output) => output,
        Err(error) => return (Vec::new(), Some(error)),
    };
    let mut lines = if args.first().is_some_and(|arg| arg == "inspect") {
        apple_inspect_summary(&output.stdout).unwrap_or_default()
    } else {
        Vec::new()
    };
    if lines.is_empty() {
        lines = String::from_utf8_lossy(&output.stdout)
            .lines()
            .chain(String::from_utf8_lossy(&output.stderr).lines())
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(max_lines)
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
    }
    if lines.is_empty() {
        lines.push("No output returned.".into());
    }
    let error =
        (!output.status.success()).then(|| format!("command exited with {}", output.status));
    (lines, error)
}

fn apple_inspect_summary(output: &[u8]) -> Option<Vec<String>> {
    let value = serde_json::from_slice::<serde_json::Value>(output).ok()?;
    let item = value
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&value);
    let configuration = item.get("configuration").unwrap_or(item);
    let status = item.get("status");
    let id = configuration
        .get("id")
        .or_else(|| item.get("id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let image = configuration
        .pointer("/image/reference")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let state = status
        .and_then(|status| status.get("state"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let os = configuration
        .pointer("/platform/os")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("linux");
    let architecture = configuration
        .pointer("/platform/architecture")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let cpus = configuration
        .pointer("/resources/cpus")
        .and_then(serde_json::Value::as_u64)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "auto".into());
    let memory = configuration
        .pointer("/resources/memoryInBytes")
        .and_then(serde_json::Value::as_u64)
        .map(|bytes| format!("{:.1} GiB", bytes as f64 / 1024.0 / 1024.0 / 1024.0))
        .unwrap_or_else(|| "auto".into());
    Some(vec![
        format!("ID: {}", id),
        format!("Image: {}", image),
        format!("State: {}", state),
        format!("Platform: {}/{}", os, architecture),
        format!("Resources: {} CPU · {}", cpus, memory),
    ])
}

fn applescript_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn open_runtime_container_terminal(runtime: &str, name: &str) -> Result<(), String> {
    let child_path = runtime_paths::build_child_path();
    let command = match runtime.trim().to_ascii_lowercase().as_str() {
        "apple" | "container" => "container",
        "docker" | "orb" | "orbstack" => "docker",
        _ => return Err(format!("Unsupported runtime `{}`.", runtime)),
    };
    let command_path = runtime_paths::find_command_path(command, &child_path)
        .ok_or_else(|| format!("Command `{}` was not found.", command))?;
    let terminal_command = format!(
        "{} exec -it {} sh -lc {}",
        runtime_paths::shell_single_quote(&command_path.display().to_string()),
        runtime_paths::shell_single_quote(name),
        runtime_paths::shell_single_quote("exec ${SHELL:-/bin/sh}")
    );
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        applescript_string(&terminal_command)
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn open_runtime_machine_terminal(runtime: &str, name: &str) -> Result<(), String> {
    if !matches!(
        runtime.trim().to_ascii_lowercase().as_str(),
        "orb" | "orbstack"
    ) {
        return Err(format!("Unsupported machine runtime `{}`.", runtime));
    }
    let child_path = runtime_paths::build_child_path();
    let command_path = runtime_paths::find_command_path("orbctl", &child_path)
        .ok_or_else(|| "Command `orbctl` was not found.".to_string())?;
    let terminal_command = format!(
        "{} run --machine {}",
        runtime_paths::shell_single_quote(&command_path.display().to_string()),
        runtime_paths::shell_single_quote(name)
    );
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        applescript_string(&terminal_command)
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn open_container_terminal(index: usize) -> Result<(), String> {
    let sessions = container_sessions::load_sessions();
    let Some(session) = sessions.get(index) else {
        return Err("Session no longer exists".into());
    };
    let command = container_sessions::terminal_command(session);
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        applescript_string(&command)
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[allow(deprecated)] // winit 0.30 compatibility path; ApplicationHandler migration is separate.
fn main() {
    if let Some(socket) = std::env::var_os("COCOA_WAY_ASKPASS_SOCKET") {
        let mut secret = String::new();
        let result = std::os::unix::net::UnixStream::connect(socket)
            .and_then(|mut stream| stream.read_to_string(&mut secret));
        match result {
            Ok(_) => {
                println!("{}", secret);
                return;
            }
            Err(_) => std::process::exit(1),
        }
    }

    if container_sessions::should_run_container_relay() {
        std::process::exit(container_sessions::run_container_relay_from_env());
    }

    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if let Some(request) = command_line_request(&arguments) {
        print_command_line_request(request);
        return;
    }

    install_display_worker_panic_report();

    // Default filter: our code at INFO, smithay/wayland noise at WARN only.
    // Override with RUST_LOG env var.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        "cocoa_way=info,smithay=warn,wayland_server=warn,wayland_client=warn".into()
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
    if let Err(error) = runtime_paths::configure_xkb_config_root() {
        log::error!("{error}");
    }
    if std::env::var_os(DISPLAY_WORKER_SLOT_ENV).is_none() {
        cleanup_stale_display_runtime_dirs();
    }
    let event_loop = EventLoop::new().unwrap();
    macos_gestures::set_event_loop_proxy(event_loop.create_proxy());
    let mut event_handler = None;
    event_loop
        .run(move |event, target| {
            if event_handler.is_none() {
                if !matches!(event, Event::Resumed) {
                    return;
                }
                match create_event_handler(target) {
                    Ok(handler) => event_handler = Some(handler),
                    Err(error) => {
                        log::error!("Cocoa-Way startup failed: {error}");
                        // SAFETY: Resumed is delivered on the AppKit main thread.
                        let mtm = unsafe { objc2_foundation::MainThreadMarker::new_unchecked() };
                        menu_bar::show_startup_error(&error, mtm);
                        target.exit();
                        return;
                    }
                }
            }
            if let Some(handler) = event_handler.as_mut() {
                handler(event, target);
            }
        })
        .unwrap();
}

#[cfg(test)]
mod display_slot_tests {
    use super::*;

    #[test]
    fn apple_image_pull_uses_plain_progress_and_selected_transport_options() {
        let (command, args) = image_pull_command(
            "container",
            "docker.io/library/ubuntu:24.04".into(),
            Some("linux/arm64"),
            Some("https"),
        )
        .unwrap();
        assert_eq!(command, "container");
        assert_eq!(
            args,
            [
                "image",
                "pull",
                "--progress",
                "plain",
                "--scheme",
                "https",
                "--platform",
                "linux/arm64",
                "docker.io/library/ubuntu:24.04",
            ]
        );
    }

    #[test]
    fn apple_image_pull_leaves_registry_scheme_to_the_runtime_by_default() {
        let (_, args) = image_pull_command(
            "container",
            "docker.io/library/ubuntu:24.04".into(),
            None,
            None,
        )
        .unwrap();
        assert!(!args.iter().any(|argument| argument == "--scheme"));
    }

    #[test]
    fn docker_image_pull_uses_docker_platform_syntax() {
        let (command, args) = image_pull_command(
            "docker",
            "ghcr.io/example/gui:latest".into(),
            Some("linux/amd64"),
            Some("http"),
        )
        .unwrap();
        assert_eq!(command, "docker");
        assert_eq!(
            args,
            [
                "pull",
                "--platform",
                "linux/amd64",
                "ghcr.io/example/gui:latest",
            ]
        );
    }

    #[test]
    fn registry_login_keeps_the_password_out_of_process_arguments() {
        let args = registry_login_args("ghcr.io", "example", Some("https"));
        assert_eq!(
            args,
            [
                "registry",
                "login",
                "--password-stdin",
                "--username",
                "example",
                "--scheme",
                "https",
                "ghcr.io",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "secret-token"));
    }

    fn session(name: &str, display: Option<&str>) -> container_sessions::ContainerSession {
        container_sessions::ContainerSession {
            name: name.into(),
            image: "example:latest".into(),
            runtime: "container".into(),
            display: display.map(str::to_owned),
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
        }
    }

    #[test]
    fn default_display_rejects_a_second_active_session() {
        let sessions = vec![session("first", Some("auto")), session("second", None)];
        assert_eq!(
            active_display_conflict_index(1, &sessions, [(0, "default".into())]),
            None
        );

        let sessions = vec![
            session("first", Some("auto")),
            session("second", Some("default")),
        ];
        assert_eq!(
            active_display_conflict_index(1, &sessions, [(0, "default".into())]),
            Some(0)
        );
    }

    #[test]
    fn relaunching_the_tracked_session_is_not_a_display_conflict() {
        let sessions = vec![session("first", Some("default"))];
        assert_eq!(
            active_display_conflict_index(0, &sessions, [(0, "default".into())]),
            None
        );
    }

    #[test]
    fn unsupported_display_does_not_claim_the_default_slot() {
        let sessions = vec![
            session("external", Some("external")),
            session("default", None),
        ];
        assert_eq!(
            active_display_conflict_index(0, &sessions, [(1, "default".into())]),
            None
        );
    }

    #[test]
    fn auto_display_uses_a_dedicated_worker_after_default_is_taken() {
        let auto = session("Second Desktop", Some("auto"));
        assert_eq!(
            choose_display_assignment(&auto, false),
            DisplayAssignment::Default
        );
        assert_eq!(
            choose_display_assignment(&auto, true),
            DisplayAssignment::Dedicated("session-second-desktop".into())
        );
    }

    #[test]
    fn named_display_is_stable_across_restarts() {
        let named = session("Desktop", Some("Research Window"));
        assert_eq!(
            choose_display_assignment(&named, false),
            DisplayAssignment::Dedicated("research-window".into())
        );
    }

    #[test]
    fn managed_display_names_are_stable_and_reserve_policy_keywords() {
        assert_eq!(
            normalize_managed_display_slot("Research Window").as_deref(),
            Ok("research-window")
        );
        for reserved in ["auto", "default", "dedicated"] {
            assert!(normalize_managed_display_slot(reserved).is_err());
        }
    }

    #[test]
    fn rootless_sessions_always_use_an_isolated_worker() {
        let mut rootless = session("Browser Window", Some("auto"));
        rootless.presentation = Some("rootless".into());

        assert_eq!(
            choose_display_assignment(&rootless, false),
            DisplayAssignment::Dedicated("rootless-browser-window".into())
        );
        assert_eq!(
            choose_display_assignment(&rootless, true),
            DisplayAssignment::Dedicated("rootless-browser-window".into())
        );
    }

    #[test]
    fn rootless_named_display_is_stable_and_namespaced() {
        let mut rootless = session("Browser", Some("Research Window"));
        rootless.presentation = Some("rootless".into());

        assert_eq!(
            choose_display_assignment(&rootless, false),
            DisplayAssignment::Dedicated("rootless-research-window".into())
        );
    }

    #[test]
    fn named_display_rejects_a_second_active_session() {
        let sessions = vec![
            session("Research", Some("Research Window")),
            session("Browser", Some("Research Window")),
        ];
        assert_eq!(
            active_display_conflict_index(1, &sessions, [(0, "research-window".into())]),
            Some(0)
        );
    }

    #[test]
    fn current_process_is_visible_to_display_worker_liveness_check() {
        assert!(process_exists(std::process::id()));
    }

    #[test]
    fn command_line_help_and_version_do_not_start_the_gui() {
        assert_eq!(
            command_line_request(&["--help".into()]),
            Some(CommandLineRequest::Help)
        );
        assert_eq!(
            command_line_request(&["-V".into()]),
            Some(CommandLineRequest::Version)
        );
        assert_eq!(command_line_request(&[]), None);
    }

    #[test]
    fn display_runtime_parent_pid_supports_marker_and_legacy_directory() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("display.parent"), "4242\n").unwrap();
        assert_eq!(display_runtime_parent_pid(directory.path()), Some(4242));
        assert_eq!(
            display_runtime_parent_pid(std::path::Path::new("/tmp/cwd-31337-8")),
            Some(31337)
        );
    }

    #[test]
    fn runtime_lifecycle_commands_use_the_native_cli() {
        assert_eq!(
            runtime_container_command("apple", "stop", "desktop"),
            Some(("container", vec!["stop".into(), "desktop".into()]))
        );
        assert_eq!(
            runtime_container_command("orbstack", "delete", "worker"),
            Some(("docker", vec!["rm".into(), "-f".into(), "worker".into()]))
        );
        assert_eq!(
            runtime_container_command("docker", "restart", "desktop"),
            Some(("docker", vec!["restart".into(), "desktop".into()]))
        );
    }

    #[test]
    fn runtime_detail_commands_match_each_cli() {
        let apple = runtime_detail_commands("container", "desktop").unwrap();
        assert_eq!(apple.command, "container");
        assert_eq!(apple.info, vec!["inspect", "desktop"]);
        assert_eq!(apple.logs, vec!["logs", "-n", "20", "desktop"]);
        assert!(apple.stats.iter().any(|arg| arg == "--no-stream"));

        let docker = runtime_detail_commands("orbstack", "desktop").unwrap();
        assert_eq!(docker.command, "docker");
        assert_eq!(docker.logs, vec!["logs", "--tail", "20", "desktop"]);
        assert!(docker.stats.iter().any(|arg| arg == "--format"));
    }

    #[test]
    fn apple_inspect_json_becomes_a_readable_summary() {
        let output = br#"[{"configuration":{"id":"desktop","image":{"reference":"example/gui:latest"},"platform":{"os":"linux","architecture":"arm64"},"resources":{"cpus":4,"memoryInBytes":4294967296}},"status":{"state":"running"}}]"#;
        let summary = apple_inspect_summary(output).unwrap();
        assert!(summary.iter().any(|line| line == "ID: desktop"));
        assert!(summary.iter().any(|line| line == "State: running"));
        assert!(summary.iter().any(|line| line.contains("4.0 GiB")));
    }

    #[test]
    fn orbstack_status_parser_distinguishes_stopped_from_running() {
        assert_eq!(parse_orbstack_running("Running"), Some(true));
        assert_eq!(parse_orbstack_running("Stopped"), Some(false));
        assert_eq!(
            parse_orbstack_running("OrbStack is not running"),
            Some(false)
        );
        assert_eq!(parse_orbstack_running("Unknown"), None);
    }
}

fn release_pressed_keys(state: &mut AppState, time: u32) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    for keycode in keyboard.pressed_keys() {
        keyboard.input(
            state,
            keycode,
            smithay::backend::input::KeyState::Released,
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::<()>::Forward,
        );
    }
}

fn activate_toplevel(
    state: &mut AppState,
    target_surface: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
) {
    let toplevels = state.toplevels.clone();
    for toplevel in toplevels {
        let active = target_surface == Some(toplevel.wl_surface());
        toplevel.with_pending_state(|pending| {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            if active {
                pending.states.set(State::Activated);
            } else {
                pending.states.unset(State::Activated);
            }
        });
        toplevel.send_configure();
    }
    if let Some(keyboard) = state.seat.get_keyboard() {
        keyboard.set_focus(state, target_surface.cloned(), SERIAL_COUNTER.next_serial());
    }
}

fn forward_keyboard_event(state: &mut AppState, event: KeyEvent, time: u32) {
    let Some(scancode) = crate::keymap::map_key(event.physical_key) else {
        return;
    };
    let key_state = match event.state {
        ElementState::Pressed => smithay::backend::input::KeyState::Pressed,
        ElementState::Released => smithay::backend::input::KeyState::Released,
    };
    let keycode = smithay::input::keyboard::Keycode::from(scancode + 8);
    if let Some(keyboard) = state.seat.get_keyboard() {
        keyboard.input(
            state,
            keycode,
            key_state,
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::<()>::Forward,
        );
    }
}

fn rootless_pointer_motion(
    state: &mut AppState,
    rootless: &mut presentation::RootlessWindow,
    position: winit::dpi::PhysicalPosition<f64>,
    time: u32,
) {
    let logical = position.to_logical::<f64>(rootless.scale_factor);
    let location =
        smithay::utils::Point::<f64, smithay::utils::Logical>::from((logical.x, logical.y));
    let delta = location - rootless.last_pointer;
    let focus = presentation::surface_under(rootless, &state.popups, location)
        .or_else(|| Some((rootless.toplevel.wl_surface().clone(), (0.0, 0.0).into())));
    let pointer = state
        .seat
        .get_pointer()
        .expect("pointer seat is configured");
    if delta.x != 0.0 || delta.y != 0.0 {
        pointer.relative_motion(
            state,
            focus.clone(),
            &smithay::input::pointer::RelativeMotionEvent {
                delta,
                delta_unaccel: delta,
                utime: u64::from(time) * 1000,
            },
        );
    }
    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.frame(state);
    rootless.last_pointer = location;
}

fn refresh_rootless_pointer_focus(
    state: &mut AppState,
    rootless: &mut presentation::RootlessWindow,
    time: u32,
) {
    let physical =
        winit::dpi::LogicalPosition::new(rootless.last_pointer.x, rootless.last_pointer.y)
            .to_physical::<f64>(rootless.scale_factor);
    rootless_pointer_motion(state, rootless, physical, time);
}

fn rootless_pointer_button(
    state: &mut AppState,
    rootless: &presentation::RootlessWindow,
    button: winit::event::MouseButton,
    element_state: ElementState,
    time: u32,
) {
    if element_state == ElementState::Pressed {
        activate_toplevel(state, Some(rootless.toplevel.wl_surface()));
    }
    let button = match button {
        winit::event::MouseButton::Left => 0x110,
        winit::event::MouseButton::Right => 0x111,
        winit::event::MouseButton::Middle => 0x112,
        winit::event::MouseButton::Back => 0x116,
        winit::event::MouseButton::Forward => 0x115,
        winit::event::MouseButton::Other(code) => u32::from(code),
    };
    let pointer = state
        .seat
        .get_pointer()
        .expect("pointer seat is configured");
    pointer.button(
        state,
        &ButtonEvent {
            button,
            state: match element_state {
                ElementState::Pressed => smithay::backend::input::ButtonState::Pressed,
                ElementState::Released => smithay::backend::input::ButtonState::Released,
            },
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.frame(state);
}

fn create_event_handler(
    target: &ActiveEventLoop,
) -> Result<impl FnMut(Event<()>, &ActiveEventLoop) + use<>, String> {
    let presentation_mode = presentation::PresentationMode::from_env();
    let display_worker_slot = std::env::var(DISPLAY_WORKER_SLOT_ENV).ok();
    let display_worker_parent = std::env::var(DISPLAY_WORKER_PARENT_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 1);
    let window_title = display_worker_slot
        .as_ref()
        .map(|slot| format!("Cocoa-Way - {}", slot))
        .unwrap_or_else(|| "Cocoa-Way".into());
    // winit 0.30 requires macOS windows to be created after the first Resumed event.
    let window_attributes = winit::window::Window::default_attributes()
        .with_title(window_title)
        .with_visible(!presentation_mode.is_rootless())
        .with_inner_size(winit::dpi::LogicalSize::new(800.0f64, 600.0f64));
    let window = target
        .create_window(window_attributes)
        .map_err(|error| format!("failed to create the Cocoa-Way window: {error}"))?;
    let mut renderer = metal_renderer::MetalRenderer::new(window)
        .map_err(|error| format!("failed to initialize Metal rendering: {error}"))?;
    if let Err(error) = macos_gestures::install_swipe_recognizer(&renderer.window) {
        log::warn!("Three-finger swipe support is unavailable: {}", error);
    }
    info!("MetalRenderer created with Metal hardware rendering");
    let mut display = Display::<AppState>::new()
        .map_err(|error| format!("failed to initialize the Wayland display: {error}"))?;
    let display_handle = display.handle();
    let (loop_signal, loop_receiver) = std::sync::mpsc::channel::<CompositorMessage>();
    let control_socket_path = if display_worker_slot.is_none() {
        match control_api::start(loop_signal.clone()) {
            Ok(path) => {
                log::info!("Local control API listening on {}", path.display());
                Some(path)
            }
            Err(error) => {
                log::error!("Local control API is unavailable: {}", error);
                None
            }
        }
    } else {
        None
    };
    if display_worker_slot.is_none() {
        diagnostics::start_resource_sampler();
    }
    let menu_signal = loop_signal.clone(); // separate sender for the menu bar
    // Use scale=1: clients render at physical pixel resolution (1600x1200).
    // This gives pixel-perfect 1:1 rendering instead of blurry 2x upscale.
    let mut state = AppState::new(
        &display_handle,
        1.0, // compositor scale=1: layout in physical pixels
        loop_signal,
        presentation_mode,
        renderer.window.inner_size().width,
        renderer.window.inner_size().height,
    )
    .map_err(|error| format!("Cocoa-Way input initialization failed: {error}"))?;
    let initial_size = renderer.window.inner_size();
    let (initial_width, initial_height) = layout::sanitize_logical_size(
        f64::from(initial_size.width),
        f64::from(initial_size.height),
    );
    let initial_mode = smithay::output::Mode {
        size: (initial_width, initial_height).into(),
        refresh: 60_000,
    };
    state.output.change_current_state(
        Some(initial_mode),
        Some(smithay::utils::Transform::Normal),
        Some(smithay::output::Scale::Integer(1)),
        Some((0, 0).into()),
    );
    state.output.set_preferred(initial_mode);
    let runtime_dir = std::env::var_os(DISPLAY_WORKER_RUNTIME_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("cocoa-way"));
    if !runtime_dir.exists() {
        std::fs::create_dir_all(&runtime_dir).map_err(|error| {
            format!(
                "failed to create the Wayland runtime directory '{}': {error}",
                runtime_dir.display()
            )
        })?;
    }
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
    }
    let listener = ListeningSocket::bind_auto("wayland", 1..10).map_err(|error| {
        format!(
            "failed to create a Wayland socket in '{}': {error}",
            runtime_dir.display()
        )
    })?;
    let socket_name = listener
        .socket_name()
        .ok_or_else(|| "the Wayland listener did not publish a socket name".to_string())?
        .to_string_lossy()
        .into_owned();
    info!("Wayland socket created: {:?}", socket_name);
    info!("XDG_RUNTIME_DIR set to: {:?}", runtime_dir);
    info!(
        "To run clients: export XDG_RUNTIME_DIR={:?} WAYLAND_DISPLAY={}",
        runtime_dir, socket_name
    );
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    }
    if let Some(ready_file) = std::env::var_os(DISPLAY_WORKER_READY_ENV) {
        let ready_file = std::path::PathBuf::from(ready_file);
        let temporary = ready_file.with_extension("tmp");
        let contents = format!("{}\n{}\n", runtime_dir.display(), socket_name);
        if let Err(error) = std::fs::write(&temporary, contents)
            .and_then(|()| std::fs::rename(&temporary, &ready_file))
        {
            log::error!("Failed to publish dedicated display readiness: {}", error);
        }
    }
    let mut loop_handle = display_handle.clone();
    std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok(Some(stream)) => {
                    use crate::state::ClientState;
                    info!("New client connected");
                    if let Err(error) = loop_handle.insert_client(
                        stream,
                        Arc::new(ClientState {
                            compositor_state: Default::default(),
                        }),
                    ) {
                        log::warn!("Could not register a Wayland client: {error}");
                    }
                }
                Ok(None) => {
                    // The Wayland listening socket is non-blocking. Without a small
                    // pause this thread spins at 100% CPU while waiting for clients.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    });
    let mut hidpi_enabled = false;

    // Will be installed in Event::Resumed (after winit's applicationDidFinishLaunching)
    let connections_for_menu = connections::load_connections();
    let container_sessions_for_menu = container_sessions::load_sessions();
    let container_event_signal = menu_signal.clone();
    let mut pending_menu: Option<std::sync::mpsc::Sender<CompositorMessage>> =
        if display_worker_slot.is_some() {
            None
        } else {
            Some(menu_signal)
        };
    let mut active_container_sessions: Vec<ActiveContainerSession> = Vec::new();
    let mut active_classic_connections: Vec<ActiveClassicConnection> = Vec::new();
    let mut pending_profile_checks = HashSet::<usize>::new();
    let mut cancelled_launch_sessions = HashSet::<usize>::new();
    let mut validated_launch_checks = HashMap::<usize, container_sessions::CheckReport>::new();
    let mut pending_display_sessions: HashMap<usize, String> = HashMap::new();
    let mut pending_launch_sessions: HashMap<usize, String> = HashMap::new();
    let mut pending_launch_workers: HashMap<usize, DisplayWorker> = HashMap::new();
    let mut managed_displays: Vec<ManagedDisplay> = Vec::new();
    let mut pending_managed_displays = std::collections::HashSet::<String>::new();
    let mut rootless_windows =
        HashMap::<winit::window::WindowId, presentation::RootlessWindow>::new();

    let mut last_mouse_pos =
        smithay::utils::Point::<f64, smithay::utils::Logical>::from((0.0, 0.0));
    let start_time = std::time::Instant::now();
    let frame_duration = std::time::Duration::from_millis(16); // ~60fps cap
    let active_poll_interval = std::time::Duration::from_millis(4);
    let idle_poll_interval = std::time::Duration::from_millis(16);
    let mut last_frame = std::time::Instant::now();
    let mut last_layout_size: (i32, i32) = (0, 0); // track last logical size sent to layout
    let mut last_render_diagnostic = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let mut blank_render_since: Option<std::time::Instant> = None;
    let mut perf_window_start = std::time::Instant::now();
    let mut perf_last_commits = state.commit_counter;
    let mut perf_last_redraws = 0u64;
    let mut perf_redraws = 0u64;
    let mut perf_late_redraws = 0u64;
    let mut perf_max_redraw_wait_ms = 0.0f64;
    let mut pending_redraw_since: Option<std::time::Instant> = None;
    let mut pending_input_sample: Option<(std::time::Instant, u64)> = None;
    let mut input_to_present_ms: Option<f64> = None;
    let mut last_parent_check = std::time::Instant::now();
    Ok(move |event: Event<()>, target: &ActiveEventLoop| {
        for gesture in macos_gestures::drain_swipe_events() {
            let time = start_time.elapsed().as_millis() as u32;
            if macos_gestures::window_number(&renderer.window) == Some(gesture.window_number) {
                state.handle_swipe_gesture(gesture.delta, gesture.phase, time);
                continue;
            }
            if let Some(rootless) = rootless_windows.values_mut().find(|rootless| {
                macos_gestures::window_number(&rootless.renderer.window)
                    == Some(gesture.window_number)
            }) {
                if gesture.phase == winit::event::TouchPhase::Started {
                    refresh_rootless_pointer_focus(&mut state, rootless, time);
                }
                state.handle_swipe_gesture(gesture.delta, gesture.phase, time);
            }
        }
        while let Ok(msg) = loop_receiver.try_recv() {
            match msg {
                CompositorMessage::GuestClipboardText(text) => {
                    state.install_guest_clipboard(text);
                }
                CompositorMessage::Maximize(max) => {
                    log::info!("Handling Maximize: {}", max);
                    renderer.window.set_maximized(max);
                }
                CompositorMessage::Fullscreen(full) => {
                    log::info!("Handling Fullscreen: {}", full);
                    if full {
                        renderer
                            .window
                            .set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
                    } else {
                        renderer.window.set_fullscreen(None);
                    }
                }
                CompositorMessage::ShowDefaultDisplay => {
                    if display_worker_slot.is_none() {
                        renderer.window.set_visible(true);
                        renderer.window.focus_window();
                    }
                }
                CompositorMessage::RootlessToplevelCreated(surface_id) => {
                    if !presentation_mode.is_rootless()
                        || rootless_windows
                            .values()
                            .any(|window| window.surface_id() == surface_id)
                    {
                        continue;
                    }
                    let Some(toplevel) = state
                        .toplevels
                        .iter()
                        .find(|toplevel| toplevel.wl_surface().id() == surface_id)
                        .cloned()
                    else {
                        continue;
                    };
                    let attributes = winit::window::Window::default_attributes()
                        .with_title(presentation::toplevel_title(&toplevel))
                        .with_visible(true)
                        .with_inner_size(winit::dpi::LogicalSize::new(960.0f64, 720.0f64));
                    let rootless_renderer = target
                        .create_window(attributes)
                        .map_err(|error| error.to_string())
                        .and_then(metal_renderer::MetalRenderer::new);
                    match rootless_renderer {
                        Ok(rootless_renderer) => {
                            if let Err(error) =
                                macos_gestures::install_swipe_recognizer(&rootless_renderer.window)
                            {
                                log::warn!(
                                    "Three-finger swipe support is unavailable for a rootless window: {}",
                                    error
                                );
                            }
                            rootless_renderer.window.set_visible(true);
                            rootless_renderer.window.request_redraw();
                            rootless_renderer.window.focus_window();
                            let scale_factor = rootless_renderer.window.scale_factor();
                            let size = rootless_renderer.window.inner_size();
                            presentation::configure_toplevel(
                                &toplevel,
                                size.width,
                                size.height,
                                scale_factor,
                                rootless_renderer.window.is_maximized(),
                                rootless_renderer.window.fullscreen().is_some(),
                            );
                            let window_id = rootless_renderer.window.id();
                            rootless_windows.insert(
                                window_id,
                                presentation::RootlessWindow {
                                    renderer: rootless_renderer,
                                    toplevel,
                                    scale_factor,
                                    last_pointer: (0.0, 0.0).into(),
                                    presented_once: false,
                                    last_geometry: None,
                                    last_render_metrics: None,
                                },
                            );
                            state.needs_redraw = true;
                            log::info!(
                                "Created rootless macOS window for surface {:?}",
                                surface_id
                            );
                        }
                        Err(error) => {
                            log::error!(
                                "Failed to create rootless window for {:?}: {}",
                                surface_id,
                                error
                            );
                            toplevel.send_close();
                        }
                    }
                }
                CompositorMessage::RootlessSurfaceDestroyed(surface_id) => {
                    for window in rootless_windows.values_mut() {
                        window.renderer.evict_texture(&surface_id);
                    }
                }
                CompositorMessage::RootlessToplevelDestroyed(surface_id) => {
                    let removed_window_numbers = rootless_windows
                        .values()
                        .filter(|window| window.surface_id() == surface_id)
                        .filter_map(|window| macos_gestures::window_number(&window.renderer.window))
                        .collect::<Vec<_>>();
                    rootless_windows.retain(|_, window| window.surface_id() != surface_id);
                    for window_number in removed_window_numbers {
                        macos_gestures::uninstall_swipe_recognizer(window_number);
                    }
                }
                CompositorMessage::RootlessToplevelTitleChanged(surface_id) => {
                    if let Some(window) = rootless_windows
                        .values()
                        .find(|window| window.surface_id() == surface_id)
                    {
                        window
                            .renderer
                            .window
                            .set_title(&presentation::toplevel_title(&window.toplevel));
                    }
                }
                CompositorMessage::RootlessMaximize { surface, maximized } => {
                    if let Some(window) = rootless_windows
                        .values_mut()
                        .find(|window| window.surface_id() == surface)
                    {
                        if presentation::honor_rootless_maximize(window.presented_once, maximized) {
                            window.renderer.window.set_maximized(maximized);
                        } else {
                            let size = window.renderer.window.inner_size();
                            presentation::configure_toplevel(
                                &window.toplevel,
                                size.width,
                                size.height,
                                window.scale_factor,
                                false,
                                false,
                            );
                            log::info!(
                                "Deferred startup maximize for rootless surface {:?} to avoid an immediate full-screen SHM workload",
                                surface
                            );
                        }
                    }
                }
                CompositorMessage::RootlessFullscreen {
                    surface,
                    fullscreen,
                } => {
                    if let Some(window) = rootless_windows
                        .values()
                        .find(|window| window.surface_id() == surface)
                    {
                        window.renderer.window.set_fullscreen(if fullscreen {
                            Some(winit::window::Fullscreen::Borderless(None))
                        } else {
                            None
                        });
                    }
                }
                CompositorMessage::RootlessMinimize(surface) => {
                    if let Some(window) = rootless_windows
                        .values()
                        .find(|window| window.surface_id() == surface)
                    {
                        window.renderer.window.set_minimized(true);
                    }
                }
                CompositorMessage::RootlessBeginMove(surface) => {
                    if let Some(window) = rootless_windows
                        .values()
                        .find(|window| window.surface_id() == surface)
                        && let Err(error) = window.renderer.window.drag_window()
                    {
                        log::debug!("Native rootless window drag was rejected: {}", error);
                    }
                }
                CompositorMessage::ToggleHiDpi => {
                    hidpi_enabled = !hidpi_enabled;
                    // Two modes:
                    //  • HiDPI (scale=2): configure clients at logical (800×600).
                    //    HiDPI-aware clients render 1600×1200 at buf_scale=2 → 1:1 sharp.
                    //  • Normal (scale=1): configure clients at physical (1600×1200).
                    //    All clients render 1600×1200 at buf_scale=1 → 1:1 sharp.
                    let sys_scale = renderer.window.scale_factor();
                    let new_scale = if hidpi_enabled { sys_scale } else { 1.0 };
                    state.scale_factor = new_scale;
                    // Advertise new output scale to clients.
                    state.output.change_current_state(
                        None,
                        None,
                        Some(smithay::output::Scale::Integer(new_scale.round() as i32)),
                        None,
                    );
                    // Recalculate layout for new logical viewport.
                    let (log_w, log_h) =
                        layout::logical_size_from_physical(state.width, state.height, new_scale);
                    state.layout.set_view_size(log_w, log_h);
                    // Relayout sends new configure to every client.
                    for tile in state.layout.tiles.iter() {
                        tile.request_size();
                    }
                    renderer.request_redraw();
                    log::info!(
                        "Mode: {} (compositor scale={}, logical={}x{})",
                        if hidpi_enabled {
                            "HiDPI 2x"
                        } else {
                            "Normal 1x"
                        },
                        new_scale as i32,
                        log_w,
                        log_h
                    );
                }
                CompositorMessage::Connect(i) => {
                    log::info!("Connecting to machine #{}", i);
                    if let Some(conn) = connections::load_connections().get(i) {
                        let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
                        let disp = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
                        if let Err(error) = start_classic_connection(
                            conn,
                            &rt,
                            &disp,
                            &mut active_classic_connections,
                        ) {
                            log::error!("Connection '{}' failed: {}", conn.name, error);
                            let mtm =
                                unsafe { objc2_foundation::MainThreadMarker::new_unchecked() };
                            menu_bar::show_connection_error(&error, mtm);
                        }
                    }
                }
                CompositorMessage::ConnectMachine(conn) => {
                    log::info!("Connecting to machine '{}'", conn.name);
                    let rt = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
                    let disp = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
                    if let Err(error) =
                        start_classic_connection(&conn, &rt, &disp, &mut active_classic_connections)
                    {
                        log::error!("Connection '{}' failed: {}", conn.name, error);
                        let mtm = unsafe { objc2_foundation::MainThreadMarker::new_unchecked() };
                        menu_bar::show_connection_error(&error, mtm);
                    }
                }
                CompositorMessage::ReloadMenu => {
                    let mtm = unsafe { objc2_foundation::MainThreadMarker::new_unchecked() };
                    menu_bar::setup_menu(
                        &connections::load_connections(),
                        &container_sessions::load_sessions(),
                        container_event_signal.clone(),
                        mtm,
                    );
                    log::info!("macOS menu bar reloaded");
                }
                CompositorMessage::DisconnectClassicConnections => {
                    log::info!(
                        "Disconnecting {} classic waypipe connection(s)",
                        active_classic_connections.len()
                    );
                    for mut connection in active_classic_connections.drain(..) {
                        let _ = connection.child.kill();
                        let _ = connection.child.wait();
                    }
                }
                CompositorMessage::CheckContainerSession(i) => {
                    log::info!("Checking container session #{}", i);
                    if pending_profile_checks.insert(i) {
                        if let Some(session) = container_sessions::load_sessions().get(i).cloned() {
                            spawn_container_session_check(
                                i,
                                session,
                                false,
                                container_event_signal.clone(),
                            );
                        } else {
                            pending_profile_checks.remove(&i);
                        }
                    }
                }
                CompositorMessage::ContainerSessionChecked {
                    index,
                    launch_after,
                    result,
                } => {
                    pending_profile_checks.remove(&index);
                    if launch_after && cancelled_launch_sessions.remove(&index) {
                        container_mode::record_launch_cancelled(
                            index,
                            "Launch cancelled before the application instance was created.",
                        );
                        continue;
                    }
                    match result {
                        Ok(report) => {
                            container_mode::record_check_success(index, &report);
                            if launch_after {
                                container_mode::record_launch_progress(
                                    index,
                                    application_model::LaunchStep::CheckImage,
                                    "Runtime, image, command, and transport checks passed",
                                );
                                validated_launch_checks.insert(index, report);
                                let _ = container_event_signal
                                    .send(CompositorMessage::StartContainerSession(index));
                            }
                        }
                        Err(error) => {
                            if error.is_apple_container_transport_blocked() {
                                log::info!("Container session #{} is blocked: {}", index, error);
                            } else {
                                log::warn!("Container session #{} check failed: {}", index, error);
                            }
                            container_mode::record_check_failure(index, &error);
                            if launch_after {
                                container_mode::record_launch_failure(index, &error);
                            }
                        }
                    }
                }
                CompositorMessage::StartContainerSession(i) => {
                    log::info!("Starting container session #{}", i);
                    cancelled_launch_sessions.remove(&i);
                    let sessions = container_sessions::load_sessions();
                    if let Some(session) = sessions.get(i) {
                        reap_exited_container_sessions(&mut active_container_sessions);
                        sync_active_container_sessions(&active_container_sessions);
                        if pending_profile_checks.contains(&i)
                            || pending_display_sessions.contains_key(&i)
                            || pending_launch_sessions.contains_key(&i)
                        {
                            log::info!(
                                "Container session #{} launch ignored because launch work is already pending",
                                i
                            );
                            continue;
                        }
                        if active_container_sessions
                            .iter()
                            .any(|active| active.index == i)
                        {
                            log::info!(
                                "Container session #{} launch ignored because it is already tracked as running",
                                i
                            );
                            container_mode::record_launch_already_running(i);
                            continue;
                        }
                        let Some(check) = validated_launch_checks.remove(&i) else {
                            pending_profile_checks.insert(i);
                            spawn_container_session_check(
                                i,
                                session.clone(),
                                true,
                                container_event_signal.clone(),
                            );
                            continue;
                        };
                        if let Some(conflict_index) = active_display_conflict_index(
                            i,
                            &sessions,
                            active_container_sessions
                                .iter()
                                .map(|active| (active.index, active.display_slot.clone()))
                                .chain(
                                    pending_display_sessions
                                        .iter()
                                        .map(|(index, slot)| (*index, slot.clone())),
                                ),
                        ) {
                            let conflict_name = sessions
                                .get(conflict_index)
                                .map(|session| session.name.as_str())
                                .unwrap_or("another session");
                            let message = format!(
                                "The requested display is already used by '{}'. Stop that session or choose another display before launching.",
                                conflict_name
                            );
                            log::warn!("Container session #{} launch blocked: {}", i, message);
                            container_mode::record_launch_blocked(i, &message);
                            continue;
                        }
                        container_mode::record_launch_progress(
                            i,
                            application_model::LaunchStep::CreateContainer,
                            "Preparing the runtime instance identity, sockets, mounts, and environment",
                        );
                        container_mode::record_launch_progress(
                            i,
                            application_model::LaunchStep::AllocateDisplay,
                            "Resolving and reserving the target Cocoa-Way display",
                        );
                        let default_in_use = active_container_sessions
                            .iter()
                            .any(|active| active.display_slot == "default");
                        let assignment = choose_display_assignment(session, default_in_use);
                        match assignment {
                            DisplayAssignment::Default => {
                                pending_launch_sessions.insert(i, "default".into());
                                spawn_container_session_on_display(
                                    i,
                                    session.clone(),
                                    check,
                                    std::env::var("XDG_RUNTIME_DIR").unwrap_or_default(),
                                    std::env::var("WAYLAND_DISPLAY").unwrap_or_default(),
                                    "default".into(),
                                    container_event_signal.clone(),
                                );
                            }
                            DisplayAssignment::Dedicated(slot) => {
                                if pending_managed_displays.contains(&slot) {
                                    let message = format!(
                                        "Managed display '{}' is still starting. Wait for it to become ready, then launch this session again.",
                                        slot
                                    );
                                    container_mode::record_launch_blocked(i, &message);
                                } else if let Some(managed) =
                                    managed_displays.iter().find(|display| display.slot == slot)
                                {
                                    pending_launch_sessions.insert(i, slot.clone());
                                    spawn_container_session_on_display(
                                        i,
                                        session.clone(),
                                        check,
                                        managed.runtime_dir.clone(),
                                        managed.display.clone(),
                                        slot,
                                        container_event_signal.clone(),
                                    );
                                } else {
                                    validated_launch_checks.insert(i, check);
                                    pending_display_sessions.insert(i, slot.clone());
                                    spawn_display_worker_async(
                                        i,
                                        slot,
                                        session.presentation_mode(),
                                        container_event_signal.clone(),
                                    );
                                }
                            }
                        }
                    }
                }
                CompositorMessage::DedicatedDisplayStarted {
                    index,
                    display_slot,
                    runtime_dir,
                    display,
                    worker_child,
                    worker_runtime_dir,
                } => {
                    let mut display_worker = DisplayWorker {
                        child: worker_child,
                        runtime_dir: worker_runtime_dir,
                    };
                    let expected_slot = pending_display_sessions.remove(&index);
                    if expected_slot.as_deref() != Some(display_slot.as_str()) {
                        log::info!(
                            "Discarding dedicated display '{}' for cancelled session #{}",
                            display_slot,
                            index
                        );
                        let _ = terminate_display_worker(&mut display_worker);
                        continue;
                    }

                    reap_exited_container_sessions(&mut active_container_sessions);
                    sync_active_container_sessions(&active_container_sessions);
                    if active_container_sessions
                        .iter()
                        .any(|active| active.index == index)
                    {
                        let _ = terminate_display_worker(&mut display_worker);
                        container_mode::record_launch_already_running(index);
                        continue;
                    }
                    if let Some(conflict) = active_container_sessions
                        .iter()
                        .find(|active| active.display_slot == display_slot)
                    {
                        let sessions = container_sessions::load_sessions();
                        let conflict_name = sessions
                            .get(conflict.index)
                            .map(|session| session.name.as_str())
                            .unwrap_or("another session");
                        let message = format!(
                            "The dedicated display '{}' became occupied by '{}'. Stop that session or launch again with another display.",
                            display_slot, conflict_name
                        );
                        let _ = terminate_display_worker(&mut display_worker);
                        container_mode::record_launch_blocked(index, &message);
                        continue;
                    }

                    let sessions = container_sessions::load_sessions();
                    let Some(session) = sessions.get(index) else {
                        let _ = terminate_display_worker(&mut display_worker);
                        validated_launch_checks.remove(&index);
                        continue;
                    };
                    let Some(check) = validated_launch_checks.remove(&index) else {
                        let _ = terminate_display_worker(&mut display_worker);
                        container_mode::record_launch_blocked(
                            index,
                            "The validated launch context expired. Launch the application again.",
                        );
                        continue;
                    };
                    pending_launch_workers.insert(index, display_worker);
                    pending_launch_sessions.insert(index, display_slot.clone());
                    spawn_container_session_on_display(
                        index,
                        session.clone(),
                        check,
                        runtime_dir,
                        display,
                        display_slot,
                        container_event_signal.clone(),
                    );
                }
                CompositorMessage::DedicatedDisplayFailed {
                    index,
                    display_slot,
                    error,
                } => {
                    if pending_display_sessions.get(&index).map(String::as_str)
                        != Some(display_slot.as_str())
                    {
                        continue;
                    }
                    pending_display_sessions.remove(&index);
                    validated_launch_checks.remove(&index);
                    log::error!(
                        "Container session #{} dedicated display '{}' failed: {}",
                        index,
                        display_slot,
                        error
                    );
                    container_mode::record_launch_blocked(index, &error);
                }
                CompositorMessage::ContainerSessionLaunchProgress {
                    index,
                    step,
                    detail,
                } => {
                    container_mode::record_launch_progress(index, step, &detail);
                }
                CompositorMessage::ContainerSessionLaunchFinished {
                    index,
                    display_slot,
                    result,
                } => {
                    let expected_slot = pending_launch_sessions.remove(&index);
                    let mut display_worker = pending_launch_workers.remove(&index);
                    if expected_slot.as_deref() != Some(display_slot.as_str()) {
                        if let Some(worker) = display_worker.as_mut() {
                            let _ = terminate_display_worker(worker);
                        }
                        if let Ok(mut report) = result {
                            let _ = report.waypipe_child.kill();
                            let _ = report.waypipe_child.wait();
                            if let Some(mut child) = report.container_child {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
                        }
                        continue;
                    }
                    let sessions = container_sessions::load_sessions();
                    let Some(session) = sessions.get(index) else {
                        if let Some(worker) = display_worker.as_mut() {
                            let _ = terminate_display_worker(worker);
                        }
                        continue;
                    };
                    let show_default_display = display_slot == "default" && result.is_ok();
                    complete_container_session_launch(
                        index,
                        session,
                        display_slot,
                        display_worker,
                        result,
                        &container_event_signal,
                        &mut active_container_sessions,
                    );
                    if show_default_display {
                        renderer.window.set_visible(true);
                        renderer.window.focus_window();
                        renderer.window.request_redraw();
                        state.needs_redraw = true;
                    }
                }
                CompositorMessage::CreateManagedDisplay(requested_slot) => {
                    reap_exited_managed_displays(
                        &mut managed_displays,
                        &mut active_container_sessions,
                    );
                    let slot = if let Some(requested_slot) = requested_slot {
                        requested_slot
                    } else {
                        next_managed_display_slot(
                            &managed_displays,
                            &pending_managed_displays,
                            &active_container_sessions,
                        )
                    };
                    if pending_managed_displays.contains(&slot)
                        || managed_displays.iter().any(|display| display.slot == slot)
                        || active_container_sessions
                            .iter()
                            .any(|session| session.display_slot == slot)
                    {
                        container_mode::record_managed_display_failure(
                            &slot,
                            "This display slot is already starting or in use.",
                        );
                        continue;
                    }
                    pending_managed_displays.insert(slot.clone());
                    container_mode::record_managed_display_starting(&slot);
                    spawn_managed_display_worker_async(slot, container_event_signal.clone());
                }
                CompositorMessage::ManagedDisplayStarted {
                    display_slot,
                    runtime_dir,
                    display,
                    worker_child,
                    worker_runtime_dir,
                } => {
                    let mut worker = DisplayWorker {
                        child: worker_child,
                        runtime_dir: worker_runtime_dir,
                    };
                    if !pending_managed_displays.remove(&display_slot) {
                        let _ = terminate_display_worker(&mut worker);
                        continue;
                    }
                    if managed_displays
                        .iter()
                        .any(|managed| managed.slot == display_slot)
                        || active_container_sessions
                            .iter()
                            .any(|active| active.display_slot == display_slot)
                    {
                        let message = format!(
                            "Display slot '{}' became occupied while the managed window was starting.",
                            display_slot
                        );
                        let _ = terminate_display_worker(&mut worker);
                        container_mode::record_managed_display_failure(&display_slot, &message);
                        continue;
                    }
                    log::info!(
                        "Managed display '{}' is ready at {}/{}",
                        display_slot,
                        runtime_dir,
                        display
                    );
                    managed_displays.push(ManagedDisplay {
                        slot: display_slot,
                        runtime_dir,
                        display,
                        worker,
                    });
                    sync_managed_displays(&managed_displays);
                }
                CompositorMessage::ManagedDisplayFailed {
                    display_slot,
                    error,
                } => {
                    if pending_managed_displays.remove(&display_slot) {
                        log::error!(
                            "Managed display '{}' failed to start: {}",
                            display_slot,
                            error
                        );
                        container_mode::record_managed_display_failure(&display_slot, &error);
                    }
                }
                CompositorMessage::CloseManagedDisplay(display_slot) => {
                    if let Some(active) = active_container_sessions
                        .iter()
                        .find(|active| active.display_slot == display_slot)
                    {
                        let sessions = container_sessions::load_sessions();
                        let session_name = sessions
                            .get(active.index)
                            .map(|session| session.name.as_str())
                            .unwrap_or("a GUI session");
                        let message = format!(
                            "Managed display '{}' is still used by '{}'. Stop the session before closing the display.",
                            display_slot, session_name
                        );
                        container_mode::record_managed_display_failure(&display_slot, &message);
                        continue;
                    }
                    if pending_managed_displays.remove(&display_slot) {
                        container_mode::record_managed_display_exit(
                            &display_slot,
                            "startup cancelled",
                        );
                        continue;
                    }
                    let Some(position) = managed_displays
                        .iter()
                        .position(|managed| managed.slot == display_slot)
                    else {
                        container_mode::record_managed_display_failure(
                            &display_slot,
                            "Display no longer exists.",
                        );
                        continue;
                    };
                    let mut managed = managed_displays.remove(position);
                    match terminate_display_worker(&mut managed.worker) {
                        Ok(()) => container_mode::record_managed_display_exit(
                            &display_slot,
                            "closed by user",
                        ),
                        Err(error) => container_mode::record_managed_display_failure(
                            &display_slot,
                            &format!("Failed to close display: {}", error),
                        ),
                    }
                    sync_managed_displays(&managed_displays);
                }
                CompositorMessage::StopContainerSession(i) => {
                    log::info!("Stopping container session #{}", i);
                    if pending_profile_checks.contains(&i)
                        || pending_display_sessions.contains_key(&i)
                        || pending_launch_sessions.contains_key(&i)
                    {
                        cancelled_launch_sessions.insert(i);
                        validated_launch_checks.remove(&i);
                        pending_display_sessions.remove(&i);
                        pending_launch_sessions.remove(&i);
                        if let Some(session) = container_sessions::load_sessions().get(i).cloned() {
                            std::thread::spawn(move || {
                                if let Err(error) =
                                    container_sessions::cleanup_named_session(&session)
                                {
                                    log::warn!(
                                        "Background cleanup for cancelled application #{} failed: {}",
                                        i,
                                        error
                                    );
                                }
                            });
                        }
                        container_mode::record_launch_cancelled(
                            i,
                            "Launch cancelled; pending runtime and display resources are being cleaned up.",
                        );
                        continue;
                    }
                    container_mode::record_stop_progress(
                        i,
                        "Ask application to exit",
                        "Requesting a graceful exit from the running application",
                    );
                    match request_graceful_container_stop(&mut active_container_sessions, i) {
                        Ok(()) => {
                            sync_active_container_sessions(&active_container_sessions);
                            container_mode::record_stop_progress(
                                i,
                                "Stop application process",
                                "Exit signal delivered; waiting up to four seconds for the application to stop",
                            );
                        }
                        Err(error) => {
                            log::warn!("Container session #{} graceful stop failed: {}", i, error);
                            container_mode::record_stop_failure(i, &error);
                        }
                    }
                }
                CompositorMessage::ForceStopContainerSession(i) => {
                    log::warn!("Force stopping container session #{}", i);
                    let had_display = active_container_sessions
                        .iter()
                        .find(|session| session.index == i)
                        .is_some_and(|session| session.display_worker.is_some());
                    container_mode::record_stop_progress(
                        i,
                        "Stop application process",
                        "Graceful exit timed out; terminating the application process",
                    );
                    match stop_active_container_session(&mut active_container_sessions, i) {
                        Ok(()) => {
                            sync_active_container_sessions(&active_container_sessions);
                            container_mode::record_stop_progress(
                                i,
                                "Stop Waypipe worker",
                                "Waypipe worker was terminated",
                            );
                            if had_display {
                                container_mode::record_stop_progress(
                                    i,
                                    "Release display",
                                    "Dedicated display resources were released",
                                );
                            }
                            cleanup_named_container_session(i);
                            container_mode::record_stop_progress(
                                i,
                                "Stop container",
                                "Named container resources were removed",
                            );
                            container_mode::record_stop_progress(
                                i,
                                "Mark instance exited",
                                "Application instance is no longer running",
                            );
                            container_mode::record_stop_success(i);
                        }
                        Err(error) => container_mode::record_stop_failure(i, &error),
                    }
                }
                CompositorMessage::OpenContainerTerminal(i) => {
                    log::info!("Opening terminal for container session #{}", i);
                    match open_container_terminal(i) {
                        Ok(()) => container_mode::record_terminal_opened(i),
                        Err(error) => {
                            log::warn!(
                                "Opening terminal for container session #{} failed: {}",
                                i,
                                error
                            );
                            container_mode::record_terminal_open_failed(i, &error);
                        }
                    }
                }
                CompositorMessage::ContainerSessionLog {
                    index,
                    source,
                    line,
                } => {
                    container_mode::record_session_log(index, &source, &line);
                }
                CompositorMessage::PullContainerImage {
                    runtime,
                    image,
                    platform,
                    scheme,
                    configure_session,
                } => match diagnostics::ensure_storage_growth_allowed() {
                    Ok(_) => {
                        container_mode::record_image_pull_started(
                            &runtime,
                            &image,
                            configure_session,
                        );
                        spawn_image_pull(
                            container_event_signal.clone(),
                            runtime,
                            image,
                            platform,
                            scheme,
                        );
                    }
                    Err(error) => {
                        container_mode::record_storage_growth_blocked("pull an image", &error)
                    }
                },
                CompositorMessage::LoginContainerRegistry {
                    server,
                    username,
                    password,
                    scheme,
                } => {
                    let action = format!("registry login {}", server);
                    container_mode::record_runtime_system_action_started("apple", &action);
                    spawn_registry_login(
                        container_event_signal.clone(),
                        server,
                        username,
                        password,
                        scheme,
                    );
                }
                CompositorMessage::LoadContainerImage { path } => {
                    match diagnostics::ensure_storage_growth_allowed() {
                        Ok(_) => {
                            container_mode::record_image_load_started(&path);
                            spawn_image_load(container_event_signal.clone(), path);
                        }
                        Err(error) => container_mode::record_storage_growth_blocked(
                            "load an OCI archive",
                            &error,
                        ),
                    }
                }
                CompositorMessage::BuildContainerImage {
                    image,
                    containerfile,
                    context,
                } => match diagnostics::ensure_storage_growth_allowed() {
                    Ok(_) => {
                        container_mode::record_image_build_started(&image, &containerfile);
                        spawn_image_build(
                            container_event_signal.clone(),
                            image,
                            containerfile,
                            context,
                        );
                    }
                    Err(error) => {
                        container_mode::record_storage_growth_blocked("build an image", &error)
                    }
                },
                CompositorMessage::StartAppleContainerSystem => {
                    container_mode::record_apple_container_system_start_started();
                    spawn_apple_container_system_start(container_event_signal.clone());
                }
                CompositorMessage::DeleteContainerImage { runtime, image } => {
                    container_mode::record_image_delete_started(&runtime, &image);
                    spawn_image_delete(container_event_signal.clone(), runtime, image);
                }
                CompositorMessage::DeleteContainerVolume { runtime, volume } => {
                    container_mode::record_volume_delete_started(&runtime, &volume);
                    spawn_volume_delete(container_event_signal.clone(), runtime, volume);
                }
                CompositorMessage::CreateContainerVolume { runtime, volume } => {
                    container_mode::record_volume_create_started(&runtime, &volume);
                    spawn_volume_create(container_event_signal.clone(), runtime, volume);
                }
                CompositorMessage::StopRuntimeContainer { runtime, name } => {
                    container_mode::record_runtime_container_action_started(
                        &runtime, &name, "stop",
                    );
                    spawn_runtime_container_action(
                        container_event_signal.clone(),
                        runtime,
                        name,
                        "stop".into(),
                    );
                }
                CompositorMessage::StartRuntimeContainer { runtime, name } => {
                    container_mode::record_runtime_container_action_started(
                        &runtime, &name, "start",
                    );
                    spawn_runtime_container_action(
                        container_event_signal.clone(),
                        runtime,
                        name,
                        "start".into(),
                    );
                }
                CompositorMessage::RestartRuntimeContainer { runtime, name } => {
                    container_mode::record_runtime_container_action_started(
                        &runtime, &name, "restart",
                    );
                    spawn_runtime_container_action(
                        container_event_signal.clone(),
                        runtime,
                        name,
                        "restart".into(),
                    );
                }
                CompositorMessage::DeleteRuntimeContainer { runtime, name } => {
                    container_mode::record_runtime_container_action_started(
                        &runtime, &name, "delete",
                    );
                    spawn_runtime_container_action(
                        container_event_signal.clone(),
                        runtime,
                        name,
                        "delete".into(),
                    );
                }
                CompositorMessage::OpenRuntimeContainerTerminal { runtime, name } => {
                    match open_runtime_container_terminal(&runtime, &name) {
                        Ok(()) => container_mode::record_runtime_container_terminal_opened(
                            &runtime, &name,
                        ),
                        Err(error) => container_mode::record_runtime_container_terminal_failed(
                            &runtime, &name, &error,
                        ),
                    }
                }
                CompositorMessage::RuntimeMachineAction {
                    runtime,
                    name,
                    action,
                } => {
                    let activity_action = format!("machine {} {}", action, name);
                    container_mode::record_runtime_system_action_started(
                        &runtime,
                        &activity_action,
                    );
                    spawn_runtime_machine_action(
                        container_event_signal.clone(),
                        runtime,
                        name,
                        action,
                    );
                }
                CompositorMessage::OpenRuntimeMachineTerminal { runtime, name } => {
                    match open_runtime_machine_terminal(&runtime, &name) {
                        Ok(()) => {
                            container_mode::record_runtime_machine_terminal_opened(&runtime, &name)
                        }
                        Err(error) => container_mode::record_runtime_machine_terminal_failed(
                            &runtime, &name, &error,
                        ),
                    }
                }
                CompositorMessage::RefreshRuntimeContainerDetails { runtime, name } => {
                    spawn_runtime_container_details(container_event_signal.clone(), runtime, name);
                }
                CompositorMessage::RuntimeSystemAction { runtime, action } => {
                    container_mode::record_runtime_system_action_started(&runtime, &action);
                    spawn_runtime_system_action(container_event_signal.clone(), runtime, action);
                }
                CompositorMessage::UseDockerContext { name } => {
                    let action = format!("context use {}", name);
                    container_mode::record_runtime_system_action_started("docker", &action);
                    spawn_docker_context_switch(container_event_signal.clone(), name);
                }
                CompositorMessage::ContainerImagePullLog {
                    runtime,
                    image,
                    line,
                } => {
                    if runtime == "load" {
                        container_mode::record_image_load_log(&image, &line);
                    } else if runtime == "build" {
                        container_mode::record_image_build_log(&image, &line);
                    } else if runtime == "system" {
                        container_mode::record_apple_container_system_start_log(&line);
                    } else if let Some(delete_runtime) = runtime.strip_prefix("delete:") {
                        container_mode::record_image_delete_log(delete_runtime, &image, &line);
                    } else {
                        container_mode::record_image_pull_log(&runtime, &image, &line);
                    }
                }
                CompositorMessage::ContainerImagePullFinished {
                    runtime,
                    image,
                    success,
                    status,
                } => {
                    if runtime == "load" {
                        container_mode::record_image_load_finished(&image, success, &status);
                    } else if runtime == "build" {
                        container_mode::record_image_build_finished(&image, success, &status);
                    } else if runtime == "system" {
                        container_mode::record_apple_container_system_start_finished(
                            success, &status,
                        );
                    } else if let Some(delete_runtime) = runtime.strip_prefix("delete:") {
                        container_mode::record_image_delete_finished(
                            delete_runtime,
                            &image,
                            success,
                            &status,
                        );
                    } else {
                        container_mode::record_image_pull_finished(
                            &runtime, &image, success, &status,
                        );
                    }
                }
                CompositorMessage::ContainerVolumeDeleteLog {
                    runtime,
                    volume,
                    line,
                } => {
                    container_mode::record_volume_delete_log(&runtime, &volume, &line);
                }
                CompositorMessage::ContainerVolumeDeleteFinished {
                    runtime,
                    volume,
                    success,
                    status,
                } => {
                    container_mode::record_volume_delete_finished(
                        &runtime, &volume, success, &status,
                    );
                }
                CompositorMessage::ContainerVolumeCreateLog {
                    runtime,
                    volume,
                    line,
                } => {
                    container_mode::record_volume_create_log(&runtime, &volume, &line);
                }
                CompositorMessage::ContainerVolumeCreateFinished {
                    runtime,
                    volume,
                    success,
                    status,
                } => {
                    container_mode::record_volume_create_finished(
                        &runtime, &volume, success, &status,
                    );
                }
                CompositorMessage::RuntimeContainerActionLog {
                    runtime,
                    name,
                    action,
                    line,
                } => {
                    container_mode::record_runtime_container_action_log(
                        &runtime, &name, &action, &line,
                    );
                }
                CompositorMessage::RuntimeContainerActionFinished {
                    runtime,
                    name,
                    action,
                    success,
                    status,
                } => {
                    container_mode::record_runtime_container_action_finished(
                        &runtime, &name, &action, success, &status,
                    );
                }
                CompositorMessage::RuntimeContainerDetailsLoaded {
                    runtime,
                    name,
                    info,
                    logs,
                    stats,
                    error,
                } => {
                    container_mode::record_runtime_container_details_loaded(
                        &runtime, &name, info, logs, stats, error,
                    );
                }
                CompositorMessage::ContainerModeCommandCacheUpdated => {
                    container_mode::record_command_cache_updated();
                }
                CompositorMessage::ContainerModeCommandCacheRefreshDue => {
                    container_mode::record_command_cache_refresh_due();
                }
                CompositorMessage::RuntimeSystemActionLog {
                    runtime,
                    action,
                    line,
                } => {
                    container_mode::record_runtime_system_action_log(&runtime, &action, &line);
                }
                CompositorMessage::RuntimeSystemActionFinished {
                    runtime,
                    action,
                    success,
                    status,
                } => {
                    container_mode::record_runtime_system_action_finished(
                        &runtime, &action, success, &status,
                    );
                }
            }
        }
        match event {
            Event::WindowEvent { window_id, event }
                if rootless_windows.contains_key(&window_id) =>
            {
                let mut rootless = rootless_windows
                    .remove(&window_id)
                    .expect("rootless window disappeared during event dispatch");
                let mut keep_window = true;
                let event_time = start_time.elapsed().as_millis() as u32;
                match event {
                    WindowEvent::Resized(size) => {
                        log::debug!(
                            "Rootless native resize {:?}: physical={}x{} logical={}x{} scale={}",
                            rootless.surface_id(),
                            size.width,
                            size.height,
                            (f64::from(size.width) / rootless.scale_factor).round() as u32,
                            (f64::from(size.height) / rootless.scale_factor).round() as u32,
                            rootless.scale_factor
                        );
                        rootless.renderer.resize(size.width, size.height);
                        presentation::configure_toplevel(
                            &rootless.toplevel,
                            size.width,
                            size.height,
                            rootless.scale_factor,
                            rootless.renderer.window.is_maximized(),
                            rootless.renderer.window.fullscreen().is_some(),
                        );
                        state.needs_redraw = true;
                    }
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        rootless.scale_factor = scale_factor;
                        rootless.renderer.set_scale_factor(scale_factor);
                        let size = rootless.renderer.window.inner_size();
                        presentation::configure_toplevel(
                            &rootless.toplevel,
                            size.width,
                            size.height,
                            scale_factor,
                            rootless.renderer.window.is_maximized(),
                            rootless.renderer.window.fullscreen().is_some(),
                        );
                        state.needs_redraw = true;
                    }
                    WindowEvent::CloseRequested => {
                        rootless.toplevel.send_close();
                        rootless.renderer.window.set_visible(false);
                    }
                    WindowEvent::Destroyed => {
                        keep_window = false;
                    }
                    WindowEvent::Focused(true) => {
                        activate_toplevel(&mut state, Some(rootless.toplevel.wl_surface()));
                    }
                    WindowEvent::Focused(false) => {
                        release_pressed_keys(&mut state, event_time);
                        if state
                            .seat
                            .get_keyboard()
                            .and_then(|keyboard| keyboard.current_focus())
                            .as_ref()
                            == Some(rootless.toplevel.wl_surface())
                        {
                            activate_toplevel(&mut state, None);
                        }
                    }
                    WindowEvent::KeyboardInput { event, .. } => {
                        if pending_input_sample.is_none() {
                            pending_input_sample =
                                Some((std::time::Instant::now(), state.commit_counter));
                        }
                        forward_keyboard_event(&mut state, event, event_time);
                    }
                    WindowEvent::CursorEntered { .. } => {
                        rootless.renderer.window.set_cursor_visible(true);
                    }
                    WindowEvent::CursorLeft { .. } => {
                        if let Some(pointer) = state.seat.get_pointer() {
                            pointer.motion(
                                &mut state,
                                None,
                                &MotionEvent {
                                    location: rootless.last_pointer,
                                    serial: SERIAL_COUNTER.next_serial(),
                                    time: event_time,
                                },
                            );
                            pointer.frame(&mut state);
                        }
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        if pending_input_sample.is_none() {
                            pending_input_sample =
                                Some((std::time::Instant::now(), state.commit_counter));
                        }
                        rootless_pointer_motion(&mut state, &mut rootless, position, event_time);
                    }
                    WindowEvent::MouseInput {
                        state: press,
                        button,
                        ..
                    } => {
                        if pending_input_sample.is_none() {
                            pending_input_sample =
                                Some((std::time::Instant::now(), state.commit_counter));
                        }
                        rootless_pointer_button(&mut state, &rootless, button, press, event_time);
                    }
                    WindowEvent::MouseWheel { delta, phase, .. } => {
                        if phase == winit::event::TouchPhase::Started {
                            refresh_rootless_pointer_focus(&mut state, &mut rootless, event_time);
                        }
                        state.handle_pointer_axis(rootless.scale_factor, delta, phase, event_time);
                    }
                    WindowEvent::PinchGesture { delta, phase, .. } => {
                        if phase == winit::event::TouchPhase::Started {
                            refresh_rootless_pointer_focus(&mut state, &mut rootless, event_time);
                        }
                        state.handle_pinch_gesture(delta, phase, event_time);
                    }
                    WindowEvent::RotationGesture { delta, phase, .. } => {
                        if phase == winit::event::TouchPhase::Started {
                            refresh_rootless_pointer_focus(&mut state, &mut rootless, event_time);
                        }
                        state.handle_rotation_gesture(delta, phase, event_time);
                    }
                    WindowEvent::RedrawRequested => {
                        let rendered =
                            presentation::render_rootless_window(&mut rootless, &state.popups);
                        if rendered > 0 && !rootless.presented_once {
                            log::info!(
                                "Presented first rootless frame for {:?} from {} surface buffer(s)",
                                rootless.surface_id(),
                                rendered
                            );
                            // AppKit may still report a freshly created window as hidden,
                            // causing the earlier focus request to be ignored. Activate once
                            // after real client content has reached the native window.
                            rootless.presented_once = true;
                            rootless.renderer.window.set_visible(true);
                            rootless.renderer.window.focus_window();
                        }
                        if rendered == 0 && rootless.toplevel.wl_surface().is_alive() {
                            log::debug!(
                                "Rootless surface {:?} has not committed a drawable buffer yet",
                                rootless.surface_id()
                            );
                        }
                        let presented_at = std::time::Instant::now();
                        if let Some(redraw_since) = pending_redraw_since.take() {
                            let wait_ms =
                                presented_at.duration_since(redraw_since).as_secs_f64() * 1000.0;
                            perf_max_redraw_wait_ms = perf_max_redraw_wait_ms.max(wait_ms);
                            if wait_ms > 25.0 {
                                perf_late_redraws = perf_late_redraws.saturating_add(1);
                            }
                        }
                        if let Some((input_at, commit_baseline)) = pending_input_sample
                            && state.commit_counter > commit_baseline
                        {
                            let sample =
                                presented_at.duration_since(input_at).as_secs_f64() * 1000.0;
                            input_to_present_ms = Some(
                                input_to_present_ms
                                    .map(|previous| previous * 0.8 + sample * 0.2)
                                    .unwrap_or(sample),
                            );
                            pending_input_sample = None;
                        }
                        perf_redraws = perf_redraws.saturating_add(1);
                        let callback_time = state.start_time.elapsed().as_millis() as u32;
                        let root_surface = rootless.surface_id();
                        for callback in state.take_frame_callbacks_for(Some(&root_surface)) {
                            callback.done(callback_time);
                        }
                    }
                    _ => {}
                }
                if keep_window {
                    rootless_windows.insert(window_id, rootless);
                } else if let Some(window_number) =
                    macos_gestures::window_number(&rootless.renderer.window)
                {
                    macos_gestures::uninstall_swipe_recognizer(window_number);
                }
            }
            Event::WindowEvent { window_id, event } if window_id == renderer.window.id() => {
                match event {
                    WindowEvent::Resized(size) => {
                        renderer.resize(size.width, size.height);
                        state.width = size.width;
                        state.height = size.height;
                        // Preserve whatever scale mode was active (HiDPI or normal).
                        let cur_scale = state.scale_factor;
                        let (width, height) = layout::sanitize_logical_size(
                            f64::from(size.width),
                            f64::from(size.height),
                        );
                        if (width as u32, height as u32) != (size.width, size.height) {
                            log::warn!(
                                "Window reported unsafe output size {}x{}; advertising {}x{} to Wayland clients",
                                size.width,
                                size.height,
                                width,
                                height
                            );
                        }
                        let mode = smithay::output::Mode {
                            size: (width, height).into(),
                            refresh: 60_000,
                        };
                        state.output.change_current_state(
                            Some(mode),
                            Some(smithay::utils::Transform::Normal),
                            Some(smithay::output::Scale::Integer(cur_scale.round() as i32)),
                            Some((0, 0).into()),
                        );
                        // Recalculate layout and tell all clients their new size.
                        let (log_w, log_h) =
                            layout::logical_size_from_physical(size.width, size.height, cur_scale);
                        state.layout.set_view_size(log_w, log_h);
                        for tile in state.layout.tiles.iter() {
                            tile.request_size();
                        }
                        state.needs_redraw = true;
                    }
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        log::info!("ScaleFactorChanged: {}", scale_factor);
                        state.update_scale_factor(scale_factor);
                        renderer.set_scale_factor(scale_factor);
                        state.needs_redraw = true;
                    }
                    WindowEvent::CloseRequested => {
                        if display_worker_slot.is_some() {
                            target.exit();
                        } else {
                            release_pressed_keys(
                                &mut state,
                                start_time.elapsed().as_millis() as u32,
                            );
                            renderer.window.set_cursor_visible(true);
                            renderer.window.set_visible(false);
                        }
                    }
                    WindowEvent::CursorEntered { .. } => {
                        if !state.layout.tiles.is_empty() {
                            renderer.window.set_cursor_visible(false);
                        }
                    }
                    WindowEvent::CursorLeft { .. } => {
                        renderer.window.set_cursor_visible(true);
                    }
                    WindowEvent::Focused(false) => {
                        if let Some(keyboard) = state.seat.get_keyboard() {
                            let pressed_keys = keyboard.pressed_keys();
                            if !pressed_keys.is_empty() {
                                log::info!(
                                    "Releasing {} held key(s) after the Cocoa-Way window lost focus",
                                    pressed_keys.len()
                                );
                            }
                            for keycode in pressed_keys {
                                keyboard.input(
                                    &mut state,
                                    keycode,
                                    smithay::backend::input::KeyState::Released,
                                    SERIAL_COUNTER.next_serial(),
                                    start_time.elapsed().as_millis() as u32,
                                    |_, _, _| FilterResult::<()>::Forward,
                                );
                            }
                        }
                    }
                    WindowEvent::Focused(true) => {}
                    WindowEvent::KeyboardInput {
                        event:
                            KeyEvent {
                                state: el_state,
                                physical_key,
                                ..
                            },
                        ..
                    } => {
                        if pending_input_sample.is_none() {
                            pending_input_sample =
                                Some((std::time::Instant::now(), state.commit_counter));
                        }
                        if let winit::keyboard::PhysicalKey::Code(key_code) = physical_key {
                            match key_code {
                                _ => {
                                    use smithay::backend::input::KeyState;
                                    use smithay::input::keyboard::Keycode;
                                    let serial = SERIAL_COUNTER.next_serial();
                                    let time = start_time.elapsed().as_millis() as u32;
                                    if let Some(keyboard) = state.seat.get_keyboard() {
                                        if let Some(scancode) = crate::keymap::map_key(physical_key)
                                        {
                                            let key_state = match el_state {
                                                ElementState::Pressed => KeyState::Pressed,
                                                ElementState::Released => KeyState::Released,
                                            };
                                            let keycode = Keycode::from(scancode + 8);
                                            keyboard.input(
                                                &mut state,
                                                keycode,
                                                key_state,
                                                serial,
                                                time,
                                                |_, modifiers, _| {
                                                    if el_state == ElementState::Pressed
                                                        && matches!(
                                                            key_code,
                                                            winit::keyboard::KeyCode::KeyC
                                                                | winit::keyboard::KeyCode::KeyV
                                                        )
                                                        && modifiers.ctrl
                                                        && modifiers.shift
                                                    {
                                                        log::info!(
                                                            "Forwarding Ctrl+Shift+{:?} to the focused Wayland client",
                                                            key_code
                                                        );
                                                    }
                                                    FilterResult::<()>::Forward
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        if pending_input_sample.is_none() {
                            pending_input_sample =
                                Some((std::time::Instant::now(), state.commit_counter));
                        }
                        let scale = state.scale_factor;
                        let logical_pos = position.to_logical::<f64>(scale);
                        log::debug!(
                            "CursorMoved: Physical({:?}) -> Logical({:?})",
                            position,
                            logical_pos
                        );
                        let serial = SERIAL_COUNTER.next_serial();
                        let pointer = state.seat.get_pointer().unwrap();
                        let cursor_logical_point =
                            smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                                logical_pos.x,
                                logical_pos.y,
                            ));
                        let delta = cursor_logical_point - last_mouse_pos;
                        if !state.layout.tiles.is_empty() {
                            renderer.window.set_cursor_visible(false);
                        }
                        if let Some(target_id) = state.start_drag_request.take() {
                            let (cur_x, cur_y) = state
                                .layout
                                .tile_for_surface(&target_id)
                                .map(|t| (t.position.x, t.position.y))
                                .unwrap_or((0, 0));
                            let offset_x = logical_pos.x - cur_x as f64;
                            let offset_y = logical_pos.y - cur_y as f64;
                            state.drag_state = Some((target_id.clone(), (offset_x, offset_y)));
                            log::info!("Drag Started for {:?}", target_id);
                        }
                        if let Some((target_id, (offset_x, offset_y))) = state.drag_state.clone() {
                            let new_x = (logical_pos.x - offset_x) as i32;
                            let new_y = (logical_pos.y - offset_y) as i32;
                            state.layout.move_tile(&target_id, new_x, new_y);
                            state.needs_redraw = true;
                            renderer.request_redraw();
                        }
                        let mut focus = None;
                        for tile in state.layout.tiles.iter().rev() {
                            let tile_x = tile.position.x as f64;
                            let tile_y = tile.position.y as f64;
                            let tile_w = tile.size.w as f64;
                            let tile_h = tile.size.h as f64;
                            if logical_pos.x >= tile_x
                                && logical_pos.x < tile_x + tile_w
                                && logical_pos.y >= tile_y
                                && logical_pos.y < tile_y + tile_h
                            {
                                let wl_surface = tile.toplevel.wl_surface();
                                let surface_location =
                                    smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                                        tile_x, tile_y,
                                    ));
                                log::debug!(
                                    "HitTest: FOUND tile {:?} at logical ({:.0}, {:.0})",
                                    wl_surface.id(),
                                    tile_x,
                                    tile_y
                                );
                                focus = Some((wl_surface.clone(), surface_location));
                                break;
                            }
                        }
                        if focus.is_none() && !state.layout.tiles.is_empty() {
                            log::debug!(
                                "HitTest: cursor at ({:.0}, {:.0}) not in any tile",
                                logical_pos.x,
                                logical_pos.y
                            );
                        }
                        let time = start_time.elapsed().as_millis() as u32;
                        // Send relative motion if the focused surface has an active lock constraint.
                        let is_locked = focus.as_ref().map(|(surface, _)| {
                            smithay::wayland::pointer_constraints::with_pointer_constraint::<crate::state::AppState, _, _>(
                                surface,
                                &pointer,
                                |constraint| {
                                    constraint.map(|c| {
                                        matches!(*c, smithay::wayland::pointer_constraints::PointerConstraint::Locked(_)) && c.is_active()
                                    }).unwrap_or(false)
                                },
                            )
                        }).unwrap_or(false);
                        if delta.x != 0.0 || delta.y != 0.0 {
                            pointer.relative_motion(
                                &mut state,
                                focus.clone(),
                                &smithay::input::pointer::RelativeMotionEvent {
                                    delta,
                                    delta_unaccel: delta,
                                    utime: time as u64 * 1000,
                                },
                            );
                        }
                        if !is_locked {
                            let event = MotionEvent {
                                location: cursor_logical_point,
                                serial,
                                time,
                            };
                            pointer.motion(&mut state, focus, &event);
                        }
                        pointer.frame(&mut state);
                        last_mouse_pos = cursor_logical_point;
                        let _ = display.flush_clients();
                    }
                    WindowEvent::MouseInput {
                        state: el_state,
                        button,
                        ..
                    } => {
                        if pending_input_sample.is_none() {
                            pending_input_sample =
                                Some((std::time::Instant::now(), state.commit_counter));
                        }
                        log::info!("MouseInput: {:?} {:?}", button, el_state);
                        let serial = SERIAL_COUNTER.next_serial();
                        let pointer = state.seat.get_pointer().unwrap();
                        let keyboard = state.seat.get_keyboard().unwrap();
                        let button_code = match button {
                            winit::event::MouseButton::Left => 0x110,
                            winit::event::MouseButton::Right => 0x111,
                            winit::event::MouseButton::Middle => 0x112,
                            _ => 0x110,
                        };
                        let p_state = match el_state {
                            ElementState::Pressed => smithay::backend::input::ButtonState::Pressed,
                            ElementState::Released => {
                                smithay::backend::input::ButtonState::Released
                            }
                        };
                        let time = start_time.elapsed().as_millis() as u32;
                        if p_state == smithay::backend::input::ButtonState::Pressed
                            && button == winit::event::MouseButton::Left
                        {
                            let mut focus_surface = None;
                            if let Some(pointer_state) = state.seat.get_pointer() {
                                if let Some(surface) = pointer_state.current_focus() {
                                    focus_surface = Some(surface);
                                }
                            }
                            if let Some(surface) = focus_surface {
                                log::info!(
                                    "Click-Focus: Setting keyboard focus to {:?}",
                                    surface.id()
                                );
                                keyboard.set_focus(&mut state, Some(surface.clone()), serial);
                                if let Some(toplevel) =
                                    state.toplevels.iter().find(|t| t.wl_surface() == &surface)
                                {
                                    toplevel.with_pending_state(|state| {
	                                        state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated);
	                                    });
                                    toplevel.send_configure();
                                    state.needs_redraw = true;
                                }
                            } else {
                                keyboard.set_focus(&mut state, None, serial);
                            }
                        }
                        if p_state == smithay::backend::input::ButtonState::Pressed
                            && button == winit::event::MouseButton::Left
                        {
                            if let Some(target_id) = state.start_drag_request.take() {
                                let (cur_x, cur_y) = state
                                    .layout
                                    .tile_for_surface(&target_id)
                                    .map(|t| (t.position.x, t.position.y))
                                    .unwrap_or((0, 0));
                                let offset_x = last_mouse_pos.x - cur_x as f64;
                                let offset_y = last_mouse_pos.y - cur_y as f64;
                                state.drag_state = Some((target_id, (offset_x, offset_y)));
                                state.needs_redraw = true;
                            }
                        }
                        if p_state == smithay::backend::input::ButtonState::Released
                            && button == winit::event::MouseButton::Left
                        {
                            state.drag_state = None;
                            state.needs_redraw = true;
                        }
                        let event = ButtonEvent {
                            button: button_code,
                            state: p_state,
                            serial,
                            time,
                        };
                        pointer.button(&mut state, &event);
                        pointer.frame(&mut state);
                    }
                    WindowEvent::MouseWheel { delta, phase, .. } => {
                        let time = start_time.elapsed().as_millis() as u32;
                        let scale_factor = state.scale_factor;
                        state.handle_pointer_axis(scale_factor, delta, phase, time);
                    }
                    WindowEvent::PinchGesture { delta, phase, .. } => {
                        let time = start_time.elapsed().as_millis() as u32;
                        state.handle_pinch_gesture(delta, phase, time);
                    }
                    WindowEvent::RotationGesture { delta, phase, .. } => {
                        let time = start_time.elapsed().as_millis() as u32;
                        state.handle_rotation_gesture(delta, phase, time);
                    }
                    WindowEvent::RedrawRequested => {
                        let (width, height) = {
                            let size = renderer.window.inner_size();
                            (size.width, size.height)
                        };
                        if width > 0 && height > 0 {
                            if width != renderer.width || height != renderer.height {
                                renderer.resize(width, height);
                            }
                            renderer.clear(0.1, 0.1, 0.15, 1.0);
                            use smithay::reexports::wayland_server::Resource;
                            let mut rendered_count = 0;
                            let before_toplevels = state.toplevels.len();
                            let before_tiles = state.layout.tiles.len();
                            for tile in state.layout.tiles.iter() {
                                if !tile.toplevel.wl_surface().is_alive() {
                                    renderer.evict_texture(&tile.toplevel.wl_surface().id());
                                }
                            }
                            state.toplevels.retain(|t| t.wl_surface().is_alive());
                            state
                                .layout
                                .tiles
                                .retain(|t| t.toplevel.wl_surface().is_alive());
                            if state.toplevels.len() != before_toplevels
                                || state.layout.tiles.len() != before_tiles
                            {
                                log::warn!(
                                    "CLEANUP: toplevels {} -> {}, tiles {} -> {}",
                                    before_toplevels,
                                    state.toplevels.len(),
                                    before_tiles,
                                    state.layout.tiles.len()
                                );
                            }
                            let scale = state.scale_factor;
                            let (logical_width, logical_height) =
                                layout::logical_size_from_physical(width, height, scale);
                            if (logical_width, logical_height) != last_layout_size {
                                last_layout_size = (logical_width, logical_height);
                                state.layout.set_view_size(logical_width, logical_height);
                            }
                            if state.layout.tiles.is_empty() {
                                log::debug!("RENDER: No tiles to render");
                            } else {
                                log::debug!("RENDER: {} tiles", state.layout.tiles.len());
                            }
                            state.popups.retain(|popup| popup.wl_surface().is_alive());
                            for tile in state.layout.tiles.iter() {
                                let wl_surface = tile.toplevel.wl_surface();
                                let x_offset = tile.position.x;
                                let y_offset = tile.position.y;
                                let phys_x = (x_offset as f64 * scale) as i32;
                                let phys_y = (y_offset as f64 * scale) as i32;
                                let phys_w = (tile.size.w as f64 * scale) as i32;
                                let phys_h = (tile.size.h as f64 * scale) as i32;
                                rendered_count += presentation::render_toplevel_tree(
                                    &mut renderer,
                                    &tile.toplevel,
                                    (x_offset, y_offset),
                                    scale,
                                );
                                rendered_count += presentation::render_toplevel_popups(
                                    &mut renderer,
                                    wl_surface,
                                    &state.popups,
                                    (x_offset, y_offset),
                                    scale,
                                );
                                let is_focused = state
                                    .seat
                                    .get_keyboard()
                                    .and_then(|k| k.current_focus())
                                    .map(|s| &s == wl_surface)
                                    .unwrap_or(false);
                                let border_width = 4;
                                // Only draw border when tile has enough margin — same NDC
                                // clipping issue as shadow if we go negative.
                                if is_focused && phys_x >= border_width && phys_y >= border_width {
                                    renderer.draw_border(
                                        phys_x - border_width,
                                        phys_y - border_width,
                                        phys_w + border_width * 2,
                                        phys_h + border_width * 2,
                                        border_width as f32,
                                    );
                                }
                            }
                            if rendered_count > 0 || state.layout.tiles.is_empty() {
                                blank_render_since = None;
                            } else {
                                let now = std::time::Instant::now();
                                let blank_since = *blank_render_since.get_or_insert(now);
                                if now.duration_since(blank_since)
                                    >= std::time::Duration::from_secs(5)
                                    && now.duration_since(last_render_diagnostic)
                                        >= std::time::Duration::from_secs(5)
                                {
                                    last_render_diagnostic = now;
                                    log::warn!(
                                        "RENDER: {} tiles have not produced a drawable surface for {}s",
                                        state.layout.tiles.len(),
                                        now.duration_since(blank_since).as_secs()
                                    );
                                }
                            }
                            if let Err(e) = renderer.swap_buffers() {
                                log::error!("Failed to swap buffers: {}", e);
                            }
                            let presented_at = std::time::Instant::now();
                            if let Some(redraw_since) = pending_redraw_since.take() {
                                let wait_ms =
                                    presented_at.duration_since(redraw_since).as_secs_f64()
                                        * 1000.0;
                                perf_max_redraw_wait_ms = perf_max_redraw_wait_ms.max(wait_ms);
                                if wait_ms > 25.0 {
                                    perf_late_redraws = perf_late_redraws.saturating_add(1);
                                }
                            }
                            if let Some((input_at, commit_baseline)) = pending_input_sample
                                && state.commit_counter > commit_baseline
                            {
                                let sample =
                                    presented_at.duration_since(input_at).as_secs_f64() * 1000.0;
                                input_to_present_ms = Some(
                                    input_to_present_ms
                                        .map(|previous| previous * 0.8 + sample * 0.2)
                                        .unwrap_or(sample),
                                );
                                pending_input_sample = None;
                            }
                            perf_redraws = perf_redraws.saturating_add(1);
                            state.needs_redraw = false;
                            let t = state.start_time.elapsed().as_millis() as u32;
                            for cb in state.take_frame_callbacks_for(None) {
                                cb.done(t);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => {
                let now = std::time::Instant::now();
                if let Some(parent_pid) = display_worker_parent
                    && now.duration_since(last_parent_check) >= std::time::Duration::from_secs(1)
                {
                    last_parent_check = now;
                    if !process_exists(parent_pid) {
                        log::info!(
                            "Dedicated display parent {} exited; closing worker",
                            parent_pid
                        );
                        target.exit();
                        return;
                    }
                }
                state.poll_host_clipboard();
                reap_classic_connections(&mut active_classic_connections);
                reap_exited_managed_displays(&mut managed_displays, &mut active_container_sessions);
                sync_managed_displays(&managed_displays);
                reap_exited_container_sessions(&mut active_container_sessions);
                sync_active_container_sessions(&active_container_sessions);
                match display.dispatch_clients(&mut state) {
                    Ok(_) => {
                        state.request_pending_guest_clipboard();
                        if let Err(error) = display.flush_clients() {
                            log::debug!("Failed to flush Wayland clients: {}", error);
                        }
                    }
                    Err(_) => {}
                }
                if state.needs_redraw && pending_redraw_since.is_none() {
                    pending_redraw_since = Some(now);
                }
                if pending_input_sample.is_some_and(|(input_at, _)| {
                    now.duration_since(input_at) >= std::time::Duration::from_millis(500)
                }) {
                    pending_input_sample = None;
                }
                if now.duration_since(perf_window_start) >= std::time::Duration::from_secs(1) {
                    let elapsed = now.duration_since(perf_window_start).as_secs_f64();
                    let redraw_delta = perf_redraws.saturating_sub(perf_last_redraws);
                    let commit_delta = state.commit_counter.saturating_sub(perf_last_commits);
                    container_mode::record_performance_snapshot(
                        redraw_delta as f64 / elapsed,
                        commit_delta as f64 / elapsed,
                        if presentation_mode.is_rootless() {
                            rootless_windows.len()
                        } else {
                            state.layout.tiles.len()
                        },
                        state.needs_redraw,
                        state.pending_frame_callbacks.len(),
                        perf_late_redraws as f64 / elapsed,
                        perf_max_redraw_wait_ms,
                        input_to_present_ms,
                    );
                    perf_window_start = now;
                    perf_last_redraws = perf_redraws;
                    perf_last_commits = state.commit_counter;
                    perf_late_redraws = 0;
                    perf_max_redraw_wait_ms = 0.0;
                }
                // Keep the short poll only while a frame or input response is in
                // flight. Merely owning a static window must not wake the worker
                // 250 times per second indefinitely.
                let poll_interval = if state.needs_redraw || pending_input_sample.is_some() {
                    active_poll_interval
                } else {
                    idle_poll_interval
                };
                if state.needs_redraw && now.duration_since(last_frame) >= frame_duration {
                    if presentation_mode.is_rootless() {
                        let dirty_roots = std::mem::take(&mut state.rootless_dirty_surfaces);
                        for window in rootless_windows.values() {
                            if dirty_roots.is_empty() || dirty_roots.contains(&window.surface_id())
                            {
                                window.renderer.request_redraw();
                            }
                        }
                        state.needs_redraw = false;
                    } else {
                        renderer.request_redraw();
                    }
                    last_frame = now;
                    target.set_control_flow(ControlFlow::WaitUntil(now + poll_interval));
                } else if state.needs_redraw {
                    target.set_control_flow(ControlFlow::WaitUntil(last_frame + frame_duration));
                } else {
                    target.set_control_flow(ControlFlow::WaitUntil(now + poll_interval));
                }
            }
            Event::Resumed => {
                // Install menu bar once, after winit's applicationDidFinishLaunching.
                if let Some(sender) = pending_menu.take() {
                    // SAFETY: Resumed always fires on the main thread
                    let mtm = unsafe { objc2_foundation::MainThreadMarker::new_unchecked() };
                    menu_bar::setup_menu(
                        &connections_for_menu,
                        &container_sessions_for_menu,
                        sender,
                        mtm,
                    );
                    // Disable macOS tab bar via NSView -> NSWindow
                    {
                        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                        if let Ok(handle) = renderer.window.window_handle() {
                            if let RawWindowHandle::AppKit(h) = handle.as_raw() {
                                let ns_view = h.ns_view.as_ptr() as *mut objc2::runtime::AnyObject;
                                // -[NSView window] returns id (@), not *mut c_void (^v)
                                let ns_win: *mut objc2::runtime::AnyObject =
                                    unsafe { objc2::msg_send![ns_view, window] };
                                if !ns_win.is_null() {
                                    menu_bar::disable_window_tabbing(
                                        ns_win as *mut std::ffi::c_void,
                                    );
                                }
                            }
                        }
                    }
                    log::info!("macOS menu bar installed");
                }
            }
            Event::LoopExiting => {
                macos_gestures::uninstall_all_swipe_recognizers();
                for mut connection in active_classic_connections.drain(..) {
                    let _ = connection.child.kill();
                    let _ = connection.child.wait();
                }
                while let Some(index) = active_container_sessions
                    .first()
                    .map(|session| session.index)
                {
                    let _ = stop_active_container_session(&mut active_container_sessions, index);
                    cleanup_named_container_session(index);
                }
                for mut managed in managed_displays.drain(..) {
                    let _ = terminate_display_worker(&mut managed.worker);
                }
                sync_managed_displays(&managed_displays);
                if display_worker_slot.is_some() {
                    let _ = std::fs::remove_dir_all(&runtime_dir);
                }
                if let Some(path) = control_socket_path.as_deref() {
                    control_api::remove_socket(path);
                }
            }
            _ => {}
        }
    })
}
