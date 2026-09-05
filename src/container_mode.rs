use std::collections::{HashMap, HashSet};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use objc2::declare_class;
use objc2::mutability::MainThreadOnly;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{ClassType, DeclaredClass, msg_send, msg_send_id, sel};
use objc2_app_kit::{
    NSAlert, NSApplicationActivationOptions, NSBackingStoreType, NSBox, NSBoxType, NSButton,
    NSColor, NSFont, NSModalResponseOK, NSOpenPanel, NSPasteboard, NSPasteboardTypeString,
    NSPopUpButton, NSRunningApplication, NSSavePanel, NSScrollView, NSSecureTextField, NSTextField,
    NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};
use serde::{Deserialize, Serialize};

use crate::application_model::{
    ApplicationInstanceSnapshot, DisplayStatus, InstanceStatus, LaunchStep, OperationTask,
    ProfileStatus, RuntimeStatus, TaskStatus, TaskStep, TaskStepStatus,
};
use crate::container_sessions::{self, ContainerSession};
use crate::messages::CompositorMessage;
use crate::runtime_paths::{build_child_path, find_command_path, shell_single_quote};

static SENDER: Mutex<Option<Sender<CompositorMessage>>> = Mutex::new(None);
static WINDOW: Mutex<Option<usize>> = Mutex::new(None);
static HANDLER: Mutex<Option<usize>> = Mutex::new(None);
static SELECTED_NAV: Mutex<usize> = Mutex::new(0);
static SELECTED_TAB: Mutex<usize> = Mutex::new(0);
static SELECTED_SESSION: Mutex<Option<usize>> = Mutex::new(None);
static ACTIVITY: Mutex<Vec<String>> = Mutex::new(Vec::new());
static SESSION_STATES: Mutex<Vec<(usize, SessionState)>> = Mutex::new(Vec::new());
static SESSION_LOGS: Mutex<Vec<(usize, Vec<String>)>> = Mutex::new(Vec::new());
static OPERATION_TASKS: Mutex<Vec<OperationTask>> = Mutex::new(Vec::new());
static AUTO_VALIDATION_REQUESTED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static IMAGE_TASK_ACTIVE: Mutex<Option<String>> = Mutex::new(None);
static IMAGE_TASK_DETAIL: Mutex<Option<String>> = Mutex::new(None);
static PENDING_PULL_SESSION: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static LAST_STREAM_REBUILD: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_PERFORMANCE_REBUILD: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_RESIZE_REBUILD: Mutex<Option<Instant>> = Mutex::new(None);
static IMAGE_CREATE_ACTIONS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static IMAGE_DELETE_ACTIONS: Mutex<Vec<ImageDeleteAction>> = Mutex::new(Vec::new());
static IMAGE_SELECT_ACTIONS: Mutex<Vec<SelectedImage>> = Mutex::new(Vec::new());
static VOLUME_DELETE_ACTIONS: Mutex<Vec<VolumeDeleteAction>> = Mutex::new(Vec::new());
static VOLUME_SELECT_ACTIONS: Mutex<Vec<SelectedVolume>> = Mutex::new(Vec::new());
static SELECTED_IMAGE: Mutex<Option<SelectedImage>> = Mutex::new(None);
static SELECTED_VOLUME: Mutex<Option<SelectedVolume>> = Mutex::new(None);
static RUNTIME_CONTAINER_ACTIONS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static RUNTIME_CONTAINER_SELECT_ACTIONS: Mutex<Vec<SelectedRuntimeContainer>> =
    Mutex::new(Vec::new());
static SELECTED_RUNTIME_CONTAINER: Mutex<Option<SelectedRuntimeContainer>> = Mutex::new(None);
static RUNTIME_CONTAINER_DETAILS: Mutex<Option<RuntimeContainerDetails>> = Mutex::new(None);
static RUNTIME_SYSTEM_STATES: Mutex<Vec<RuntimeSystemState>> = Mutex::new(Vec::new());
static DOCKER_CONTEXT_ACTIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static ORBSTACK_MACHINE_ACTIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static PERFORMANCE: Mutex<Option<PerformanceSnapshot>> = Mutex::new(None);
static ACTIVE_SESSIONS: Mutex<Vec<ActiveSessionSnapshot>> = Mutex::new(Vec::new());
static MANAGED_DISPLAYS: Mutex<Vec<ManagedDisplaySnapshot>> = Mutex::new(Vec::new());
static PENDING_MANAGED_DISPLAYS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CLOSING_MANAGED_DISPLAYS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MANAGED_DISPLAY_LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
static MANAGED_DISPLAY_ACTIONS: Mutex<Vec<ManagedDisplaySnapshot>> = Mutex::new(Vec::new());
static APPLE_COMPATIBILITY_CACHE: Mutex<Option<(Instant, AppleContainerCompatibility)>> =
    Mutex::new(None);
static UI_COMMAND_CACHE: LazyLock<Mutex<HashMap<String, UiCommandCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static UI_COMMAND_REFRESHING: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static SCROLL_POSITIONS: LazyLock<Mutex<HashMap<String, f64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LIST_SCROLL_VIEW: Mutex<Option<TrackedScrollView>> = Mutex::new(None);
static DETAIL_SCROLL_VIEW: Mutex<Option<TrackedScrollView>> = Mutex::new(None);
static SUMMARY_FPS_LABEL: Mutex<Option<usize>> = Mutex::new(None);
static LIVE_DISPLAY_FPS_LABELS: Mutex<Vec<LiveDisplayFpsLabel>> = Mutex::new(Vec::new());
static COMMAND_CACHE_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);
static TASK_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const UI_COMMAND_CACHE_TTL: Duration = Duration::from_secs(10);
const UI_COMMAND_LOADING: &str = "Loading runtime data...";
const APPLE_CONTAINER_RELEASES_URL: &str = "https://github.com/apple/container/releases/latest";

const NAV_SESSIONS: usize = 0;
const NAV_IMAGES: usize = 1;
const NAV_VOLUMES: usize = 2;
const NAV_DISPLAYS: usize = 3;
const NAV_APPLE_CONTAINER: usize = 4;
const NAV_DOCKER: usize = 5;
const NAV_ORBSTACK: usize = 6;
const NAV_ACTIVITY: usize = 7;
const NAV_COMMANDS: usize = 8;
const NAV_RUNNING: usize = 9;

#[derive(Clone)]
struct SessionState {
    profile: ProfileStatus,
    instance: Option<InstanceStatus>,
    detail: String,
    failed_step: Option<LaunchStep>,
    force_stop_available: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct ActiveSessionSnapshot {
    instance: ApplicationInstanceSnapshot,
    display_runtime_dir: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
struct ManagedDisplaySnapshot {
    slot: String,
    runtime_dir: String,
    display: String,
    pid: u32,
}

struct LiveDisplayFpsLabel {
    slot: String,
    base: String,
    pointer: usize,
}

#[derive(Clone)]
struct SelectedImage {
    runtime: String,
    runtime_key: String,
    reference: String,
    label: String,
}

#[derive(Clone)]
struct ImageDeleteAction {
    runtime: String,
    reference: String,
    image_id: Option<String>,
}

#[derive(Clone)]
struct SelectedVolume {
    runtime: String,
    runtime_key: String,
    name: String,
    label: String,
}

#[derive(Clone)]
struct VolumeDeleteAction {
    runtime: String,
    name: String,
}

#[derive(Default)]
struct VolumeUsage {
    referenced_profiles: Vec<String>,
    mounted_containers: Vec<String>,
    loading: bool,
    error: Option<String>,
}

struct VolumeMetadata {
    kind: String,
    size: String,
    created: String,
}

#[derive(Clone)]
struct TrackedScrollView {
    pointer: usize,
    key: String,
}

#[derive(Clone, PartialEq, Eq)]
struct SelectedRuntimeContainer {
    runtime: String,
    name: String,
    label: String,
    running: bool,
}

#[derive(Clone)]
struct RuntimeContainerDetails {
    runtime: String,
    name: String,
    info: Vec<String>,
    logs: Vec<String>,
    stats: Vec<String>,
    error: Option<String>,
}

#[derive(Clone)]
struct RuntimeSystemState {
    runtime: String,
    status: RuntimeStatus,
    detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PerformanceSnapshot {
    redraw_fps: f64,
    commits_per_second: f64,
    tiles: usize,
    dirty: bool,
    pending_frame_callbacks: usize,
    late_redraws_per_second: f64,
    max_redraw_wait_ms: f64,
    input_to_present_ms: Option<f64>,
    sampled_at_unix_ms: u128,
}

#[derive(Clone)]
struct AppleContainerCompatibility {
    cli_version: String,
    api_version: String,
    system_status: String,
    publish_socket: bool,
    stats_json: bool,
    summary: String,
    detail: String,
}

const APPLE_CONTAINER_TRANSPORT_V2_MINIMUM: (u64, u64, u64) = (1, 1, 0);
const APPLE_CONTAINER_SECURITY_BASELINE: (u64, u64, u64) = (1, 3, 1);

#[derive(Clone)]
struct UiCommandCacheEntry {
    completed_at: Instant,
    result: Result<Arc<Output>, String>,
}

struct AddSessionDefaults {
    name: String,
    runtime: String,
    display: String,
    presentation: String,
    profile: String,
    image: String,
    command: String,
    mounts: String,
    env: String,
    audio: bool,
}

impl Default for AddSessionDefaults {
    fn default() -> Self {
        Self {
            name: String::new(),
            runtime: String::new(),
            display: String::new(),
            presentation: String::new(),
            profile: String::new(),
            image: String::new(),
            command: String::new(),
            mounts: String::new(),
            env: String::new(),
            audio: true,
        }
    }
}

fn send(msg: CompositorMessage) {
    if let Ok(g) = SENDER.lock() {
        if let Some(tx) = g.as_ref() {
            let _ = tx.send(msg);
        }
    }
}

fn schedule_automatic_profile_validation(sessions: &[ContainerSession]) {
    for (index, _) in sessions.iter().enumerate() {
        if session_state(index).is_some() {
            continue;
        }
        let should_validate = {
            let mut requested = AUTO_VALIDATION_REQUESTED.lock().unwrap();
            if requested.contains(&index) {
                false
            } else {
                requested.push(index);
                true
            }
        };
        if should_validate {
            send(CompositorMessage::CheckContainerSession(index));
        }
    }
}

fn invalidate_profile_validation(index: usize) {
    SESSION_STATES
        .lock()
        .unwrap()
        .retain(|(stored, _)| *stored != index);
    AUTO_VALIDATION_REQUESTED
        .lock()
        .unwrap()
        .retain(|stored| *stored != index);
}

fn invalidate_all_profile_validation() {
    SESSION_STATES.lock().unwrap().clear();
    AUTO_VALIDATION_REQUESTED.lock().unwrap().clear();
}

fn remember_launch_request(index: usize) {
    let sessions = container_sessions::load_sessions();
    let message = match sessions.get(index) {
        Some(session) => {
            clear_session_logs(index);
            start_launch_task(index, &session.name);
            set_session_state(
                index,
                "Starting",
                format!(
                    "Launching {} through {} with command: {}",
                    session.name,
                    runtime_label(&session.runtime),
                    session_display_command(session)
                ),
            );
            format!(
                "Launch requested: {} via {} ({})",
                session.name,
                runtime_label(&session.runtime),
                session_display_command(session)
            )
        }
        None => format!(
            "Launch requested for missing application profile #{}",
            index + 1
        ),
    };
    push_activity(message);
}

fn push_activity(message: String) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let mut activity = ACTIVITY.lock().unwrap();
    activity.push(format!("[{}] {}", now, message));
    if activity.len() > 50 {
        let overflow = activity.len() - 50;
        activity.drain(0..overflow);
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn start_operation_task(
    key: impl Into<String>,
    operation: impl Into<String>,
    subject: impl Into<String>,
    steps: impl IntoIterator<Item = impl Into<String>>,
) -> u64 {
    let key = key.into();
    let now = now_unix_ms();
    let id = TASK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let task = OperationTask {
        id,
        key: key.clone(),
        operation: operation.into(),
        subject: subject.into(),
        status: TaskStatus::Queued,
        steps: steps
            .into_iter()
            .map(|name| TaskStep {
                name: name.into(),
                status: TaskStepStatus::Pending,
                detail: None,
            })
            .collect(),
        detail: None,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
    };
    let mut tasks = OPERATION_TASKS.lock().unwrap();
    tasks.retain(|existing| existing.key != key || !existing.status.is_active());
    tasks.push(task);
    if tasks.len() > 100 {
        let overflow = tasks.len() - 100;
        tasks.drain(0..overflow);
    }
    id
}

fn update_operation_task_step(
    key: &str,
    step: &str,
    status: TaskStepStatus,
    detail: Option<String>,
) {
    let mut tasks = OPERATION_TASKS.lock().unwrap();
    let Some(task) = tasks.iter_mut().rev().find(|task| task.key == key) else {
        return;
    };
    task.status = if status == TaskStepStatus::Failed {
        TaskStatus::Failed
    } else {
        TaskStatus::Running
    };
    if let Some(target_index) = task.steps.iter().position(|target| target.name == step) {
        if status == TaskStepStatus::Running {
            for previous in &mut task.steps[..target_index] {
                if matches!(
                    previous.status,
                    TaskStepStatus::Pending | TaskStepStatus::Running
                ) {
                    previous.status = TaskStepStatus::Completed;
                }
            }
        }
        let target = &mut task.steps[target_index];
        target.status = status;
        target.detail = detail.clone();
    }
    task.detail = detail;
    task.updated_at_unix_ms = now_unix_ms();
}

fn finish_operation_task(key: &str, status: TaskStatus, detail: impl Into<String>) {
    let detail = detail.into();
    let mut tasks = OPERATION_TASKS.lock().unwrap();
    let Some(task) = tasks.iter_mut().rev().find(|task| task.key == key) else {
        return;
    };
    task.status = status;
    task.detail = Some(detail);
    task.updated_at_unix_ms = now_unix_ms();
    if status == TaskStatus::Completed {
        for step in &mut task.steps {
            if step.status != TaskStepStatus::Failed {
                step.status = TaskStepStatus::Completed;
            }
        }
    } else if status == TaskStatus::Failed {
        if let Some(step) = task
            .steps
            .iter_mut()
            .find(|step| step.status == TaskStepStatus::Running)
        {
            step.status = TaskStepStatus::Failed;
        }
    }
}

fn operation_tasks_snapshot() -> Vec<OperationTask> {
    OPERATION_TASKS.lock().unwrap().clone()
}

fn latest_operation_task(key: &str) -> Option<OperationTask> {
    OPERATION_TASKS
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|task| task.key == key)
        .cloned()
}

pub(crate) fn control_operation_tasks(limit: usize) -> Vec<OperationTask> {
    let tasks = OPERATION_TASKS.lock().unwrap();
    let start = tasks.len().saturating_sub(limit);
    tasks[start..].to_vec()
}

fn active_task_count() -> usize {
    OPERATION_TASKS
        .lock()
        .unwrap()
        .iter()
        .filter(|task| task.status.is_active())
        .count()
}

fn launch_task_key(index: usize) -> String {
    format!("launch-profile-{index}")
}

fn stop_task_key(index: usize) -> String {
    format!("stop-profile-{index}")
}

fn runtime_task_key(runtime: &str) -> String {
    format!("runtime-{}", normalized_runtime_key(runtime))
}

fn resource_task_key(kind: &str, action: &str, runtime: &str, name: &str) -> String {
    format!(
        "{}-{}-{}-{}",
        kind,
        action,
        normalized_runtime_key(runtime),
        name
    )
}

fn normalized_runtime_key(runtime: &str) -> String {
    match runtime.trim().to_ascii_lowercase().as_str() {
        "apple" | "container" | "apple-container" | "apple container" => "apple".into(),
        "orb" | "orbstack" => "orbstack".into(),
        _ => "docker".into(),
    }
}

fn set_runtime_system_state(runtime: &str, status: RuntimeStatus, detail: impl Into<String>) {
    let runtime = normalized_runtime_key(runtime);
    let detail = detail.into();
    let mut states = RUNTIME_SYSTEM_STATES.lock().unwrap();
    if let Some(state) = states.iter_mut().find(|state| state.runtime == runtime) {
        state.status = status;
        state.detail = detail;
    } else {
        states.push(RuntimeSystemState {
            runtime,
            status,
            detail,
        });
    }
}

fn runtime_system_state(runtime: &str) -> Option<RuntimeSystemState> {
    let runtime = normalized_runtime_key(runtime);
    RUNTIME_SYSTEM_STATES
        .lock()
        .unwrap()
        .iter()
        .find(|state| state.runtime == runtime)
        .cloned()
}

fn start_launch_task(index: usize, name: &str) {
    start_operation_task(
        launch_task_key(index),
        "Launch application",
        name,
        LaunchStep::ALL.map(LaunchStep::label),
    );
}

pub fn record_launch_progress(index: usize, step: LaunchStep, detail: &str) {
    let key = launch_task_key(index);
    update_operation_task_step(
        &key,
        step.label(),
        TaskStepStatus::Running,
        Some(detail.into()),
    );
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|profile| profile.name.as_str())
        .unwrap_or("Application");
    if let Some((_, state)) = SESSION_STATES
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(stored, _)| *stored == index)
    {
        state.profile = ProfileStatus::Ready;
        state.instance = Some(InstanceStatus::Starting);
        state.detail = detail.into();
        state.failed_step = None;
        state.force_stop_available = false;
    }
    push_activity(format!("{}: {}", name, detail));
    unsafe {
        refresh_window_without_focus_throttled(Duration::from_millis(100));
    }
}

pub fn record_launch_cancelled(index: usize, detail: &str) {
    let key = launch_task_key(index);
    finish_operation_task(&key, TaskStatus::Cancelled, detail.to_string());
    set_session_state(index, "Stopped", detail.into());
    push_activity(detail.into());
    unsafe {
        rebuild_window();
    }
}

pub fn record_force_stop_available(index: usize) {
    if let Some((_, state)) = SESSION_STATES
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(stored, _)| *stored == index)
    {
        state.force_stop_available = true;
        state.detail =
            "The application did not exit before the timeout. Force Stop is now available.".into();
    }
    push_activity(format!(
        "Application #{} did not exit gracefully; Force Stop is available",
        index + 1
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_stop_progress(index: usize, step: &str, detail: &str) {
    update_operation_task_step(
        &stop_task_key(index),
        step,
        TaskStepStatus::Running,
        Some(detail.into()),
    );
    push_activity(detail.into());
    unsafe {
        refresh_window_without_focus_throttled(Duration::from_millis(100));
    }
}

fn set_image_task_active(message: impl Into<String>) {
    *IMAGE_TASK_ACTIVE.lock().unwrap() = Some(message.into());
    *IMAGE_TASK_DETAIL.lock().unwrap() = None;
}

fn clear_image_task_active() {
    *IMAGE_TASK_ACTIVE.lock().unwrap() = None;
    *IMAGE_TASK_DETAIL.lock().unwrap() = None;
}

fn set_image_task_detail(message: impl Into<String>) {
    *IMAGE_TASK_DETAIL.lock().unwrap() = Some(message.into());
}

fn image_task_active() -> Option<(String, Option<String>)> {
    IMAGE_TASK_ACTIVE
        .lock()
        .unwrap()
        .clone()
        .map(|message| (message, IMAGE_TASK_DETAIL.lock().unwrap().clone()))
}

pub fn record_performance_snapshot(
    redraw_fps: f64,
    commits_per_second: f64,
    tiles: usize,
    dirty: bool,
    pending_frame_callbacks: usize,
    late_redraws_per_second: f64,
    max_redraw_wait_ms: f64,
    input_to_present_ms: Option<f64>,
) {
    let snapshot = PerformanceSnapshot {
        redraw_fps,
        commits_per_second,
        tiles,
        dirty,
        pending_frame_callbacks,
        late_redraws_per_second,
        max_redraw_wait_ms,
        input_to_present_ms,
        sampled_at_unix_ms: now_unix_ms(),
    };
    publish_worker_performance(&snapshot);
    *PERFORMANCE.lock().unwrap() = Some(snapshot.clone());
    if let Some(pointer) = *SUMMARY_FPS_LABEL.lock().unwrap() {
        let summary = summary_performance_snapshot().unwrap_or_else(|| snapshot.clone());
        unsafe {
            let label = &*(pointer as *const NSTextField);
            let _: () = msg_send![label, setStringValue:
                &*NSString::from_str(&display_fps_state(&summary))];
        }
    }
    update_live_display_fps_labels();
    let should_refresh = SELECTED_NAV
        .lock()
        .map(|nav| *nav == NAV_ACTIVITY)
        .unwrap_or(false);
    if should_refresh {
        unsafe {
            refresh_window_without_focus_throttled(Duration::from_secs(2));
        }
    }
}

fn performance_snapshot() -> Option<PerformanceSnapshot> {
    PERFORMANCE.lock().unwrap().clone()
}

fn display_fps_state(snapshot: &PerformanceSnapshot) -> String {
    if snapshot.redraw_fps < 0.05 && snapshot.commits_per_second < 0.05 {
        "0.0 fps · idle".into()
    } else {
        format!("{:.1} fps", snapshot.redraw_fps)
    }
}

fn display_fps_text(base: &str, snapshot: Option<&PerformanceSnapshot>) -> String {
    snapshot
        .map(|snapshot| format!("{base} · {}", display_fps_state(snapshot)))
        .unwrap_or_else(|| base.to_string())
}

fn register_live_display_fps_label(
    label: &Retained<NSTextField>,
    slot: impl Into<String>,
    base: impl Into<String>,
) {
    LIVE_DISPLAY_FPS_LABELS
        .lock()
        .unwrap()
        .push(LiveDisplayFpsLabel {
            slot: slot.into(),
            base: base.into(),
            pointer: Retained::as_ptr(label) as usize,
        });
}

fn update_live_display_fps_labels() {
    let snapshots = control_display_performance()
        .into_iter()
        .map(
            |(
                slot,
                redraw_fps,
                commits_per_second,
                late_redraws_per_second,
                max_redraw_wait_ms,
                input_to_present_ms,
                sampled_at_unix_ms,
            )| {
                (
                    slot,
                    PerformanceSnapshot {
                        redraw_fps,
                        commits_per_second,
                        tiles: 0,
                        dirty: false,
                        pending_frame_callbacks: 0,
                        late_redraws_per_second,
                        max_redraw_wait_ms,
                        input_to_present_ms,
                        sampled_at_unix_ms,
                    },
                )
            },
        )
        .collect::<HashMap<_, _>>();
    for live in LIVE_DISPLAY_FPS_LABELS.lock().unwrap().iter() {
        let text = display_fps_text(&live.base, snapshots.get(&live.slot));
        unsafe {
            let label = &*(live.pointer as *const NSTextField);
            let _: () = msg_send![label, setStringValue: &*NSString::from_str(&text)];
        }
    }
}

fn publish_worker_performance(snapshot: &PerformanceSnapshot) {
    let Some(runtime_dir) = std::env::var_os("COCOA_WAY_DISPLAY_RUNTIME_DIR") else {
        return;
    };
    let runtime_dir = std::path::PathBuf::from(runtime_dir);
    let destination = runtime_dir.join("display-performance.json");
    let temporary = runtime_dir.join("display-performance.tmp");
    let Ok(payload) = serde_json::to_vec(snapshot) else {
        return;
    };
    if std::fs::write(&temporary, payload).is_ok() {
        let _ = std::fs::rename(temporary, destination);
    }
}

fn worker_performance_snapshot(runtime_dir: &str) -> Option<PerformanceSnapshot> {
    let payload =
        std::fs::read(std::path::Path::new(runtime_dir).join("display-performance.json")).ok()?;
    let snapshot = serde_json::from_slice::<PerformanceSnapshot>(&payload).ok()?;
    (now_unix_ms().saturating_sub(snapshot.sampled_at_unix_ms) <= 5_000).then_some(snapshot)
}

fn active_session_performance(index: usize) -> Option<PerformanceSnapshot> {
    let active = active_session(index)?;
    performance_for_active_session(&active)
}

fn performance_for_active_session(active: &ActiveSessionSnapshot) -> Option<PerformanceSnapshot> {
    active
        .display_runtime_dir
        .as_deref()
        .and_then(worker_performance_snapshot)
        .or_else(|| {
            (active.instance.display_slot == "default")
                .then(performance_snapshot)
                .flatten()
        })
}

fn summary_performance_snapshot() -> Option<PerformanceSnapshot> {
    if let Some(index) = *SELECTED_SESSION.lock().unwrap()
        && let Some(snapshot) = active_session_performance(index)
    {
        return Some(snapshot);
    }
    active_sessions_snapshot()
        .into_iter()
        .filter_map(|active| {
            active
                .display_runtime_dir
                .as_deref()
                .and_then(worker_performance_snapshot)
        })
        .max_by(|left, right| left.redraw_fps.total_cmp(&right.redraw_fps))
        .or_else(performance_snapshot)
}

pub(crate) fn control_performance_snapshot()
-> Option<(f64, f64, usize, bool, usize, f64, f64, Option<f64>)> {
    performance_snapshot().map(|snapshot| {
        (
            snapshot.redraw_fps,
            snapshot.commits_per_second,
            snapshot.tiles,
            snapshot.dirty,
            snapshot.pending_frame_callbacks,
            snapshot.late_redraws_per_second,
            snapshot.max_redraw_wait_ms,
            snapshot.input_to_present_ms,
        )
    })
}

pub(crate) fn control_display_performance() -> Vec<(String, f64, f64, f64, f64, Option<f64>, u128)>
{
    let mut displays = Vec::new();
    if let Some(snapshot) = performance_snapshot() {
        displays.push((
            "default".into(),
            snapshot.redraw_fps,
            snapshot.commits_per_second,
            snapshot.late_redraws_per_second,
            snapshot.max_redraw_wait_ms,
            snapshot.input_to_present_ms,
            snapshot.sampled_at_unix_ms,
        ));
    }
    for active in active_sessions_snapshot() {
        if active.instance.display_slot == "default"
            || displays
                .iter()
                .any(|(slot, ..)| slot == &active.instance.display_slot)
        {
            continue;
        }
        if let Some(snapshot) = performance_for_active_session(&active) {
            displays.push((
                active.instance.display_slot,
                snapshot.redraw_fps,
                snapshot.commits_per_second,
                snapshot.late_redraws_per_second,
                snapshot.max_redraw_wait_ms,
                snapshot.input_to_present_ms,
                snapshot.sampled_at_unix_ms,
            ));
        }
    }
    for display in managed_displays_snapshot() {
        if displays.iter().any(|(slot, ..)| slot == &display.slot) {
            continue;
        }
        if let Some(snapshot) = worker_performance_snapshot(&display.runtime_dir) {
            displays.push((
                display.slot,
                snapshot.redraw_fps,
                snapshot.commits_per_second,
                snapshot.late_redraws_per_second,
                snapshot.max_redraw_wait_ms,
                snapshot.input_to_present_ms,
                snapshot.sampled_at_unix_ms,
            ));
        }
    }
    displays
}

pub fn record_active_container_sessions(
    sessions: Vec<(
        u64,
        usize,
        u128,
        Option<u32>,
        u32,
        String,
        Option<u32>,
        Option<String>,
    )>,
) {
    let snapshots = sessions
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
                display_runtime_dir,
            )| {
                ActiveSessionSnapshot {
                    instance: ApplicationInstanceSnapshot {
                        id: instance_id,
                        profile_index,
                        status: InstanceStatus::Running,
                        started_at_unix_ms,
                        container_pid,
                        waypipe_pid,
                        display_slot,
                        display_pid,
                    },
                    display_runtime_dir,
                }
            },
        )
        .collect::<Vec<_>>();
    let changed = {
        let mut active = ACTIVE_SESSIONS.lock().unwrap();
        if *active == snapshots {
            false
        } else {
            *active = snapshots;
            true
        }
    };
    if !changed {
        return;
    }
    unsafe {
        refresh_window_without_focus_throttled(Duration::from_millis(500));
    }
}

pub fn record_managed_display_starting(slot: &str) {
    {
        let mut pending = PENDING_MANAGED_DISPLAYS.lock().unwrap();
        if !pending.iter().any(|candidate| candidate == slot) {
            pending.push(slot.into());
        }
    }
    *MANAGED_DISPLAY_LAST_ERROR.lock().unwrap() = None;
    push_activity(format!("Creating managed display: {}", slot));
    unsafe {
        refresh_window_without_focus_throttled(Duration::from_millis(100));
    }
}

pub fn record_managed_displays(displays: Vec<(String, String, String, u32)>) {
    let snapshots = displays
        .into_iter()
        .map(|(slot, runtime_dir, display, pid)| ManagedDisplaySnapshot {
            slot,
            runtime_dir,
            display,
            pid,
        })
        .collect::<Vec<_>>();
    let changed = {
        let mut current = MANAGED_DISPLAYS.lock().unwrap();
        if *current == snapshots {
            false
        } else {
            *current = snapshots.clone();
            true
        }
    };
    if !changed {
        return;
    }
    let active_slots = snapshots
        .iter()
        .map(|display| display.slot.as_str())
        .collect::<Vec<_>>();
    PENDING_MANAGED_DISPLAYS
        .lock()
        .unwrap()
        .retain(|slot| !active_slots.contains(&slot.as_str()));
    CLOSING_MANAGED_DISPLAYS
        .lock()
        .unwrap()
        .retain(|slot| active_slots.contains(&slot.as_str()));
    unsafe {
        refresh_window_without_focus_throttled(Duration::from_millis(100));
    }
}

pub fn record_managed_display_failure(slot: &str, error: &str) {
    PENDING_MANAGED_DISPLAYS
        .lock()
        .unwrap()
        .retain(|candidate| candidate != slot);
    CLOSING_MANAGED_DISPLAYS
        .lock()
        .unwrap()
        .retain(|candidate| candidate != slot);
    let message = format!("Managed display '{}': {}", slot, error);
    *MANAGED_DISPLAY_LAST_ERROR.lock().unwrap() = Some(message.clone());
    push_activity(message);
    unsafe {
        refresh_window_without_focus_throttled(Duration::from_millis(100));
    }
}

pub fn record_managed_display_exit(slot: &str, reason: &str) {
    PENDING_MANAGED_DISPLAYS
        .lock()
        .unwrap()
        .retain(|candidate| candidate != slot);
    CLOSING_MANAGED_DISPLAYS
        .lock()
        .unwrap()
        .retain(|candidate| candidate != slot);
    push_activity(format!("Managed display '{}' closed: {}", slot, reason));
    unsafe {
        refresh_window_without_focus_throttled(Duration::from_millis(100));
    }
}

fn managed_displays_snapshot() -> Vec<ManagedDisplaySnapshot> {
    MANAGED_DISPLAYS.lock().unwrap().clone()
}

fn pending_managed_displays_snapshot() -> Vec<String> {
    PENDING_MANAGED_DISPLAYS.lock().unwrap().clone()
}

fn closing_managed_displays_snapshot() -> Vec<String> {
    CLOSING_MANAGED_DISPLAYS.lock().unwrap().clone()
}

pub(crate) fn control_managed_displays() -> Vec<(
    String,
    DisplayStatus,
    Option<String>,
    Option<String>,
    Option<u32>,
    usize,
)> {
    let active = active_sessions_snapshot();
    let closing = closing_managed_displays_snapshot();
    let mut displays = pending_managed_displays_snapshot()
        .into_iter()
        .map(|slot| (slot, DisplayStatus::Allocating, None, None, None, 0))
        .collect::<Vec<_>>();
    displays.extend(managed_displays_snapshot().into_iter().map(|display| {
        let attachments = active
            .iter()
            .filter(|session| session.instance.display_slot == display.slot)
            .count();
        let status = if closing.iter().any(|slot| slot == &display.slot) {
            DisplayStatus::Closing
        } else if attachments > 0 {
            DisplayStatus::Attached
        } else {
            DisplayStatus::Free
        };
        (
            display.slot,
            status,
            Some(display.runtime_dir),
            Some(display.display),
            Some(display.pid),
            attachments,
        )
    }));
    displays
}

pub(crate) fn control_runtime_states() -> Vec<(String, RuntimeStatus, String)> {
    RUNTIME_SYSTEM_STATES
        .lock()
        .unwrap()
        .iter()
        .map(|state| (state.runtime.clone(), state.status, state.detail.clone()))
        .collect()
}

fn active_session(index: usize) -> Option<ActiveSessionSnapshot> {
    ACTIVE_SESSIONS
        .lock()
        .unwrap()
        .iter()
        .find(|session| session.instance.profile_index == index)
        .cloned()
}

fn active_sessions_snapshot() -> Vec<ActiveSessionSnapshot> {
    ACTIVE_SESSIONS.lock().unwrap().clone()
}

pub(crate) fn control_active_sessions()
-> Vec<(u64, usize, u128, Option<u32>, u32, String, Option<u32>)> {
    active_sessions_snapshot()
        .into_iter()
        .map(|session| {
            (
                session.instance.id,
                session.instance.profile_index,
                session.instance.started_at_unix_ms,
                session.instance.container_pid,
                session.instance.waypipe_pid,
                session.instance.display_slot,
                session.instance.display_pid,
            )
        })
        .collect()
}

pub(crate) fn control_session_state(index: usize) -> Option<(String, String)> {
    SESSION_STATES
        .lock()
        .unwrap()
        .iter()
        .find(|(stored, _)| *stored == index)
        .map(|(_, state)| (session_state_label(state).to_string(), state.detail.clone()))
}

fn active_display_conflict(index: usize) -> Option<String> {
    let sessions = container_sessions::load_sessions();
    let requested = sessions.get(index)?;
    let active_sessions = ACTIVE_SESSIONS.lock().unwrap();
    let default_in_use = active_sessions.iter().any(|active| {
        active.instance.profile_index != index && active.instance.display_slot == "default"
    });
    let requested_target = session_display_target(requested);
    let requested_slot = match requested_target.as_str() {
        "auto" if !default_in_use => "default".to_string(),
        "auto" | "dedicated" => {
            format!("session-{}", display_slot_slug(&requested.name))
        }
        "default" => "default".to_string(),
        named => display_slot_slug(named),
    };
    active_sessions
        .iter()
        .find(|active| {
            active.instance.profile_index != index && active.instance.display_slot == requested_slot
        })
        .and_then(|active| {
            sessions
                .get(active.instance.profile_index)
                .map(|session| session.name.clone())
        })
}

unsafe fn rebuild_window_throttled(interval: Duration) -> bool {
    let now = Instant::now();
    let mut last = LAST_STREAM_REBUILD.lock().unwrap();
    let should_rebuild = last
        .map(|previous| now.duration_since(previous) >= interval)
        .unwrap_or(true);
    if should_rebuild {
        *last = Some(now);
        unsafe {
            rebuild_window();
        }
    }
    should_rebuild
}

unsafe fn refresh_window_without_focus_throttled(interval: Duration) {
    let now = Instant::now();
    let mut last = LAST_PERFORMANCE_REBUILD.lock().unwrap();
    let should_rebuild = last
        .map(|previous| now.duration_since(previous) >= interval)
        .unwrap_or(true);
    if should_rebuild {
        *last = Some(now);
        unsafe {
            refresh_window_without_focus();
        }
    }
}

unsafe fn refresh_window_for_resize(interval: Duration) {
    let now = Instant::now();
    let mut last = LAST_RESIZE_REBUILD.lock().unwrap();
    if last
        .map(|previous| now.duration_since(previous) < interval)
        .unwrap_or(false)
    {
        return;
    }
    *last = Some(now);
    unsafe {
        refresh_window_without_focus();
    }
}

fn activity_snapshot() -> Vec<String> {
    ACTIVITY.lock().unwrap().clone()
}

pub(crate) fn control_activity_snapshot(limit: usize) -> Vec<String> {
    let activity = ACTIVITY.lock().unwrap();
    let start = activity.len().saturating_sub(limit);
    activity[start..].to_vec()
}

fn push_session_log(index: usize, source: &str, line: &str) {
    let mut logs = SESSION_LOGS.lock().unwrap();
    let formatted = format!("[{}] {}", source, clean_session_log_line(line));
    if let Some((_, lines)) = logs.iter_mut().find(|(stored, _)| *stored == index) {
        lines.push(formatted);
        if lines.len() > 200 {
            let overflow = lines.len() - 200;
            lines.drain(0..overflow);
        }
    } else {
        logs.push((index, vec![formatted]));
    }
}

fn clean_session_log_line(line: &str) -> String {
    let stripped = strip_ansi_sequences(line);
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    if is_niri_locale_warning(&collapsed) {
        "niri: locale1 watcher is unavailable in this container; this is non-fatal when the desktop is running.".into()
    } else {
        collapsed
    }
}

fn is_niri_locale_warning(line: &str) -> bool {
    line.contains("niri::dbus")
        && line.contains("locale1 watcher")
        && line.contains("No such file or directory")
}

fn strip_ansi_sequences(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                chars.next();
                while let Some(next) = chars.next() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }

        if ch == '[' {
            let mut code = String::new();
            while matches!(chars.peek(), Some(next) if next.is_ascii_digit() || *next == ';') {
                if let Some(next) = chars.next() {
                    code.push(next);
                }
            }
            if !code.is_empty() && matches!(chars.peek(), Some('m')) {
                chars.next();
                continue;
            }
            output.push('[');
            output.push_str(&code);
            continue;
        }

        output.push(ch);
    }

    output
}

fn clear_session_logs(index: usize) {
    let mut logs = SESSION_LOGS.lock().unwrap();
    if let Some((_, lines)) = logs.iter_mut().find(|(stored, _)| *stored == index) {
        lines.clear();
    }
}

fn normalize_profile(profile: &str) -> Option<String> {
    let value = profile.trim().to_ascii_lowercase();
    let normalized = match value.as_str() {
        "" => return None,
        "desktop" | "niri" | "niri-desktop" => "niri",
        "app" | "single" | "single-app" => "single-app",
        "debug" | "shell" | "sh" => "shell",
        other => other,
    };
    Some(normalized.into())
}

fn session_logs(index: usize) -> Vec<String> {
    SESSION_LOGS
        .lock()
        .unwrap()
        .iter()
        .find(|(stored, _)| *stored == index)
        .map(|(_, lines)| lines.clone())
        .unwrap_or_default()
}

pub(crate) fn control_session_logs(index: usize, limit: usize) -> Vec<String> {
    let logs = session_logs(index);
    let start = logs.len().saturating_sub(limit);
    logs[start..].to_vec()
}

fn smoke_image_reference() -> &'static str {
    "localhost/cocoa-way-niri:latest"
}

fn preferred_gui_image_reference() -> String {
    let child_path = build_child_path();
    let references = apple_container_image_rows(&child_path)
        .into_iter()
        .filter_map(|row| row.reference)
        .collect::<Vec<_>>();

    references
        .iter()
        .find(|reference| reference.as_str() == smoke_image_reference())
        .or_else(|| {
            references
                .iter()
                .find(|reference| reference.contains("cocoa-way-niri"))
        })
        .cloned()
        .unwrap_or_else(|| smoke_image_reference().into())
}

fn smoke_containerfile_path() -> &'static str {
    "examples/container-images/Containerfile.niri"
}

fn smoke_build_context() -> &'static str {
    "."
}

fn default_gui_runtime_args(runtime: &str, profile: Option<&str>) -> Vec<String> {
    if matches!(runtime.trim(), "container" | "apple" | "apple-container") {
        let desktop = matches!(profile, Some("niri" | "desktop"));
        let (memory, shm) = if desktop {
            ("4G", "1G")
        } else {
            ("2G", "512M")
        };
        ["--memory", memory, "--shm-size", shm, "--cpus", "4"]
            .into_iter()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    }
}

fn request_smoke_image_build() {
    if !allow_storage_growth("build the example image") {
        return;
    }
    push_activity(format!(
        "Example image build requested: {}",
        smoke_image_reference()
    ));
    send(CompositorMessage::BuildContainerImage {
        image: smoke_image_reference().into(),
        containerfile: smoke_containerfile_path().into(),
        context: smoke_build_context().into(),
    });
}

fn smoke_session() -> ContainerSession {
    ContainerSession {
        name: "Niri Desktop".into(),
        image: smoke_image_reference().into(),
        runtime: "container".into(),
        display: Some("auto".into()),
        presentation: Some("desktop".into()),
        profile: Some("niri".into()),
        app: None,
        command: Some("niri".into()),
        socket: None,
        container_socket: None,
        waypipe_path: None,
        waypipe_compress: None,
        waypipe_threads: None,
        audio: true,
        runtime_args: default_gui_runtime_args("container", Some("niri")),
        mounts: Vec::new(),
        env: Vec::new(),
    }
}

fn add_or_select_smoke_session() {
    let sessions = container_sessions::load_sessions();
    if let Some(index) = sessions
        .iter()
        .position(|session| session.image == smoke_image_reference())
    {
        *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
        *SELECTED_SESSION.lock().unwrap() = Some(index);
        push_activity("Selected existing example session.".into());
        return;
    }

    let session = smoke_session();
    match container_sessions::append_session(&session) {
        Ok(()) => {
            let sessions = container_sessions::load_sessions();
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            let index = sessions.len().checked_sub(1);
            *SELECTED_SESSION.lock().unwrap() = index;
            if let Some(index) = index {
                invalidate_profile_validation(index);
            }
            push_activity(format!("Restored example session: {}", session.image));
        }
        Err(error) => {
            let message = format!("Failed to restore example session: {}", error);
            push_activity(message.clone());
            show_error_alert(&message);
        }
    }
}

fn state_from_legacy_label(label: &str) -> (ProfileStatus, Option<InstanceStatus>) {
    match label {
        "Starting" | "Checking" => (ProfileStatus::Ready, Some(InstanceStatus::Starting)),
        "Running" => (ProfileStatus::Ready, Some(InstanceStatus::Running)),
        "Stopping" => (ProfileStatus::Ready, Some(InstanceStatus::Stopping)),
        "Stopped" | "Exited" => (ProfileStatus::Ready, Some(InstanceStatus::Exited)),
        "Error" | "Blocked" | "Failed" => (ProfileStatus::Invalid, Some(InstanceStatus::Failed)),
        _ => (ProfileStatus::Ready, None),
    }
}

fn session_state_label(state: &SessionState) -> &'static str {
    state
        .instance
        .map(InstanceStatus::label)
        .unwrap_or_else(|| state.profile.label())
}

fn set_session_state(index: usize, label: &'static str, detail: String) {
    let (profile, instance) = state_from_legacy_label(label);
    let mut states = SESSION_STATES.lock().unwrap();
    if let Some((_, state)) = states.iter_mut().find(|(stored, _)| *stored == index) {
        state.profile = profile;
        state.instance = instance;
        state.detail = detail;
        if instance != Some(InstanceStatus::Stopping) {
            state.force_stop_available = false;
        }
    } else {
        states.push((
            index,
            SessionState {
                profile,
                instance,
                detail,
                failed_step: None,
                force_stop_available: false,
            },
        ));
    }
}

fn session_state(index: usize) -> Option<SessionState> {
    SESSION_STATES
        .lock()
        .unwrap()
        .iter()
        .find(|(stored, _)| *stored == index)
        .map(|(_, state)| state.clone())
}

fn apple_container_gui_transport_ready() -> bool {
    true
}

fn session_has_apple_transport_block(session: &ContainerSession) -> bool {
    container_sessions::is_apple_container_session(session)
        && !apple_container_gui_transport_ready()
}

fn apple_transport_blocked_detail(session: &ContainerSession) -> String {
    format!(
        "{} uses Apple Container. Image and volume management are wired, but GUI launch needs a dedicated Apple Container transport before this profile can start.",
        session.name
    )
}

fn session_can_stop(state: Option<&SessionState>) -> bool {
    state
        .and_then(|state| state.instance)
        .map(InstanceStatus::is_active)
        .unwrap_or(false)
}

fn session_is_launch_busy(state: Option<&SessionState>) -> bool {
    state
        .and_then(|state| state.instance)
        .map(InstanceStatus::is_active)
        .unwrap_or(false)
}

fn checked_instance_status(
    runtime_running: bool,
    tracked_by_this_process: bool,
) -> Option<InstanceStatus> {
    (runtime_running && tracked_by_this_process).then_some(InstanceStatus::Running)
}

pub fn record_launch_success(index: usize, report: &container_sessions::LaunchReport) {
    invalidate_ui_command_cache();
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let detail = format!(
        "{} is running. Runtime: {}; container: {}; command: {}; host socket: {}; container socket: {}; waypipe pid: {}",
        name,
        report.runtime,
        report.container_name,
        report.command,
        report.host_socket,
        report.container_socket,
        report.waypipe_child.id()
    );
    set_session_state(index, "Running", detail.clone());
    finish_operation_task(
        &launch_task_key(index),
        TaskStatus::Completed,
        format!("{} is running", name),
    );
    push_activity(format!("Started: {}", detail));
    unsafe {
        rebuild_window();
    }
}

pub fn record_launch_already_running(index: usize) {
    let sessions = container_sessions::load_sessions();
    let detail = sessions
        .get(index)
        .map(|session| {
            format!(
                "{} is already running. Stop it before launching again.",
                session.name
            )
        })
        .unwrap_or_else(|| format!("Session #{} is already running.", index + 1));
    set_session_state(index, "Running", detail.clone());
    push_activity(format!("Launch ignored: {}", detail));
    unsafe {
        rebuild_window();
    }
}

pub fn record_launch_blocked(index: usize, detail: &str) {
    *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
    *SELECTED_SESSION.lock().unwrap() = Some(index);
    set_session_state(index, "Blocked", detail.into());
    finish_operation_task(
        &launch_task_key(index),
        TaskStatus::Failed,
        detail.to_string(),
    );
    push_activity(format!("Launch blocked: {}", detail));
    unsafe {
        rebuild_window();
    }
}

pub fn record_check_success(index: usize, report: &container_sessions::CheckReport) {
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let tracked = active_session(index).is_some();
    let instance = checked_instance_status(report.running, tracked);
    let status = if instance == Some(InstanceStatus::Running) {
        "running and tracked by this Cocoa-Way process"
    } else if report.running {
        "ready; an untracked instance from an earlier run will be replaced on launch"
    } else {
        "ready"
    };
    let detail = format!(
        "{} is {}. Runtime: {}; container: {}; image: {}; command: {}; waypipe: {}; runtime binary: {}",
        name,
        status,
        report.runtime,
        report.container_name,
        report.image,
        report.command,
        report.waypipe,
        report.runtime_binary
    );
    set_session_state(
        index,
        instance.map(InstanceStatus::label).unwrap_or("Ready"),
        detail.clone(),
    );
    AUTO_VALIDATION_REQUESTED
        .lock()
        .unwrap()
        .retain(|stored| *stored != index);
    push_activity(format!("Profile validated: {}", detail));
    unsafe {
        rebuild_window();
    }
}

pub fn record_check_failure(index: usize, error: &container_sessions::LaunchError) {
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let detail = format!("{} check failed: {}", name, error);
    *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
    *SELECTED_SESSION.lock().unwrap() = Some(index);
    let label = if error.is_container_already_running() {
        "Running"
    } else if error.is_unsupported_display() {
        "Blocked"
    } else {
        "Error"
    };
    set_session_state(index, label, detail.clone());
    if let Some((_, state)) = SESSION_STATES
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(stored, _)| *stored == index)
    {
        state.profile = ProfileStatus::Invalid;
        state.instance = None;
        state.failed_step = Some(LaunchStep::ValidateProfile);
    }
    AUTO_VALIDATION_REQUESTED
        .lock()
        .unwrap()
        .retain(|stored| *stored != index);
    push_activity(detail);
    unsafe {
        rebuild_window();
    }
}

pub fn record_launch_failure(index: usize, error: &container_sessions::LaunchError) {
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let detail = format!("{} failed to start: {}", name, error);
    *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
    *SELECTED_SESSION.lock().unwrap() = Some(index);
    let label = if error.is_container_already_running() {
        "Running"
    } else if error.is_unsupported_display() {
        "Blocked"
    } else {
        "Error"
    };
    set_session_state(index, label, detail.clone());
    if let Some((_, state)) = SESSION_STATES
        .lock()
        .unwrap()
        .iter_mut()
        .find(|(stored, _)| *stored == index)
    {
        state.instance = Some(InstanceStatus::Failed);
    }
    finish_operation_task(&launch_task_key(index), TaskStatus::Failed, detail.clone());
    push_activity(detail);
    unsafe {
        rebuild_window();
    }
}

pub fn record_session_log(index: usize, source: &str, line: &str) {
    push_session_log(index, source, line);
    push_activity(format!("{}: {}", source, clean_session_log_line(line)));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_image_pull_started(runtime: &str, image: &str, configure_session: bool) {
    let key = resource_task_key("image", "pull", runtime, image);
    start_operation_task(
        &key,
        "Pull Image",
        image,
        ["Resolve runtime", "Transfer image", "Refresh inventory"],
    );
    update_operation_task_step(
        &key,
        "Transfer image",
        TaskStepStatus::Running,
        Some(format!("Pulling from {}.", runtime_label(runtime))),
    );
    set_image_task_active(format!("Pulling {} image {}...", runtime, image));
    if configure_session {
        PENDING_PULL_SESSION
            .lock()
            .unwrap()
            .push((runtime.to_string(), image.to_string()));
    }
    push_activity(format!("Pull started: {} image {}", runtime, image));
    unsafe {
        rebuild_window();
    }
}

pub fn record_image_pull_log(runtime: &str, image: &str, line: &str) {
    let line = clean_session_log_line(line);
    if !line.is_empty() {
        set_image_task_detail(line.clone());
        update_operation_task_step(
            &resource_task_key("image", "pull", runtime, image),
            "Transfer image",
            TaskStepStatus::Running,
            Some(line.clone()),
        );
    }
    push_activity(format!("pull {} {}: {}", runtime, image, line));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_image_pull_finished(runtime: &str, image: &str, success: bool, status: &str) {
    invalidate_ui_command_cache();
    let configure_session = {
        let mut pending = PENDING_PULL_SESSION.lock().unwrap();
        pending
            .iter()
            .position(|(pending_runtime, pending_image)| {
                pending_runtime == runtime && pending_image == image
            })
            .map(|index| pending.remove(index))
            .is_some()
    };
    clear_image_task_active();
    let state = if success { "finished" } else { "failed" };
    finish_operation_task(
        &resource_task_key("image", "pull", runtime, image),
        if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        },
        status,
    );
    push_activity(format!(
        "Pull {}: {} image {} ({})",
        state, runtime, image, status
    ));
    unsafe {
        rebuild_window();
        if success && configure_session {
            show_session_dialog_for_image(runtime, image);
        }
    }
}

pub fn record_image_load_started(path: &str) {
    let key = resource_task_key("image", "load", "apple", path);
    start_operation_task(
        &key,
        "Import OCI Image",
        path,
        ["Read archive", "Import image", "Refresh inventory"],
    );
    update_operation_task_step(
        &key,
        "Import image",
        TaskStepStatus::Running,
        Some("Loading the OCI archive into Apple Container.".into()),
    );
    set_image_task_active(format!("Loading OCI archive {}...", path));
    push_activity(format!("Image load started: {}", path));
    unsafe {
        rebuild_window();
    }
}

pub fn record_image_load_log(path: &str, line: &str) {
    update_operation_task_step(
        &resource_task_key("image", "load", "apple", path),
        "Import image",
        TaskStepStatus::Running,
        Some(clean_session_log_line(line)),
    );
    push_activity(format!("load {}: {}", path, line));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_image_load_finished(path: &str, success: bool, status: &str) {
    invalidate_ui_command_cache();
    clear_image_task_active();
    let state = if success { "finished" } else { "failed" };
    finish_operation_task(
        &resource_task_key("image", "load", "apple", path),
        if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        },
        status,
    );
    push_activity(format!("Image load {}: {} ({})", state, path, status));
    unsafe {
        rebuild_window();
    }
}

pub fn record_image_build_started(image: &str, containerfile: &str) {
    let key = resource_task_key("image", "build", "apple", image);
    start_operation_task(
        &key,
        "Build Image",
        image,
        [
            "Read Containerfile",
            "Build layers",
            "Tag image",
            "Refresh inventory",
        ],
    );
    update_operation_task_step(
        &key,
        "Build layers",
        TaskStepStatus::Running,
        Some(format!("Building from {containerfile}.")),
    );
    set_image_task_active(format!("Building {} from {}...", image, containerfile));
    push_activity(format!(
        "Image build started: {} from {}",
        image, containerfile
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_image_build_log(image: &str, line: &str) {
    update_operation_task_step(
        &resource_task_key("image", "build", "apple", image),
        "Build layers",
        TaskStepStatus::Running,
        Some(clean_session_log_line(line)),
    );
    push_activity(format!("build {}: {}", image, line));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_image_build_finished(image: &str, success: bool, status: &str) {
    invalidate_ui_command_cache();
    clear_image_task_active();
    let state = if success { "finished" } else { "failed" };
    finish_operation_task(
        &resource_task_key("image", "build", "apple", image),
        if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        },
        status,
    );
    push_activity(format!("Image build {}: {} ({})", state, image, status));
    unsafe {
        rebuild_window();
    }
}

pub fn record_storage_growth_blocked(action: &str, error: &str) {
    clear_image_task_active();
    push_activity(format!("Storage protection blocked {}: {}", action, error));
    unsafe {
        rebuild_window();
    }
}

fn allow_storage_growth(action: &str) -> bool {
    match crate::diagnostics::ensure_storage_growth_allowed() {
        Ok(_) => true,
        Err(error) => {
            record_storage_growth_blocked(action, &error);
            show_error_alert(&error);
            false
        }
    }
}

pub fn record_apple_container_system_start_started() {
    set_runtime_system_state(
        "apple",
        RuntimeStatus::Starting,
        "Starting the Apple Container runtime.",
    );
    let key = runtime_task_key("apple");
    start_operation_task(
        &key,
        "Start Runtime",
        "Apple Container",
        ["Run runtime command", "Refresh runtime health"],
    );
    update_operation_task_step(
        &key,
        "Run runtime command",
        TaskStepStatus::Running,
        Some("Running `container system start`.".into()),
    );
    push_activity("Apple Container system start requested.".into());
    unsafe {
        rebuild_window();
    }
}

pub fn record_apple_container_system_start_log(line: &str) {
    push_activity(format!("container system start: {}", line));
    unsafe {
        rebuild_window();
    }
}

pub fn record_apple_container_system_start_finished(success: bool, status: &str) {
    invalidate_ui_command_cache();
    let state = if success { "finished" } else { "failed" };
    set_runtime_system_state(
        "apple",
        if success {
            RuntimeStatus::Ready
        } else {
            RuntimeStatus::Failed
        },
        status,
    );
    finish_operation_task(
        &runtime_task_key("apple"),
        if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        },
        status,
    );
    push_activity(format!(
        "Apple Container system start {} ({})",
        state, status
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_image_delete_started(runtime: &str, image: &str) {
    let key = resource_task_key("image", "delete", runtime, image);
    start_operation_task(
        &key,
        "Delete Image",
        image,
        ["Check dependencies", "Delete image", "Refresh inventory"],
    );
    update_operation_task_step(
        &key,
        "Delete image",
        TaskStepStatus::Running,
        Some(format!("Deleting from {}.", runtime_label(runtime))),
    );
    set_image_task_active(format!("Deleting {} image {}...", runtime, image));
    push_activity(format!("Image delete started: {} {}", runtime, image));
    unsafe {
        rebuild_window();
    }
}

pub fn record_image_delete_log(runtime: &str, image: &str, line: &str) {
    update_operation_task_step(
        &resource_task_key("image", "delete", runtime, image),
        "Delete image",
        TaskStepStatus::Running,
        Some(clean_session_log_line(line)),
    );
    push_activity(format!("delete image {}: {}", image, line));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_image_delete_finished(runtime: &str, image: &str, success: bool, status: &str) {
    invalidate_ui_command_cache();
    clear_image_task_active();
    let state = if success { "finished" } else { "failed" };
    finish_operation_task(
        &resource_task_key("image", "delete", runtime, image),
        if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        },
        status,
    );
    push_activity(format!("Image delete {}: {} ({})", state, image, status));
    unsafe {
        rebuild_window();
    }
}

pub fn record_volume_delete_started(runtime: &str, volume: &str) {
    let key = resource_task_key("volume", "delete", runtime, volume);
    start_operation_task(
        &key,
        "Delete Volume",
        volume,
        ["Check usage", "Delete volume", "Refresh inventory"],
    );
    update_operation_task_step(
        &key,
        "Delete volume",
        TaskStepStatus::Running,
        Some(format!("Deleting from {}.", runtime_label(runtime))),
    );
    push_activity(format!("Volume delete started: {} {}", runtime, volume));
    unsafe {
        rebuild_window();
    }
}

pub fn record_volume_delete_log(runtime: &str, volume: &str, line: &str) {
    update_operation_task_step(
        &resource_task_key("volume", "delete", runtime, volume),
        "Delete volume",
        TaskStepStatus::Running,
        Some(clean_session_log_line(line)),
    );
    push_activity(format!("delete volume {}: {}", volume, line));
    unsafe {
        rebuild_window();
    }
}

pub fn record_volume_delete_finished(runtime: &str, volume: &str, success: bool, status: &str) {
    invalidate_ui_command_cache();
    let state = if success { "finished" } else { "failed" };
    finish_operation_task(
        &resource_task_key("volume", "delete", runtime, volume),
        if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        },
        status,
    );
    push_activity(format!("Volume delete {}: {} ({})", state, volume, status));
    unsafe {
        rebuild_window();
    }
}

pub fn record_volume_create_started(runtime: &str, volume: &str) {
    let key = resource_task_key("volume", "create", runtime, volume);
    start_operation_task(
        &key,
        "Create Volume",
        volume,
        ["Validate name", "Create volume", "Refresh inventory"],
    );
    update_operation_task_step(
        &key,
        "Create volume",
        TaskStepStatus::Running,
        Some(format!("Creating in {}.", runtime_label(runtime))),
    );
    push_activity(format!("Volume create started: {} {}", runtime, volume));
    unsafe {
        rebuild_window();
    }
}

pub fn record_volume_create_log(runtime: &str, volume: &str, line: &str) {
    update_operation_task_step(
        &resource_task_key("volume", "create", runtime, volume),
        "Create volume",
        TaskStepStatus::Running,
        Some(clean_session_log_line(line)),
    );
    push_activity(format!("create volume {}: {}", volume, line));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_volume_create_finished(runtime: &str, volume: &str, success: bool, status: &str) {
    invalidate_ui_command_cache();
    let state = if success { "finished" } else { "failed" };
    finish_operation_task(
        &resource_task_key("volume", "create", runtime, volume),
        if success {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        },
        status,
    );
    push_activity(format!("Volume create {}: {} ({})", state, volume, status));
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_container_action_started(runtime: &str, name: &str, action: &str) {
    push_activity(format!(
        "{} container {} started: {}",
        runtime_label(runtime),
        action,
        name
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_container_action_log(runtime: &str, name: &str, action: &str, line: &str) {
    push_activity(format!(
        "{} {} {}: {}",
        runtime_label(runtime),
        action,
        name,
        line
    ));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_runtime_container_action_finished(
    runtime: &str,
    name: &str,
    action: &str,
    success: bool,
    status: &str,
) {
    invalidate_ui_command_cache();
    let state = if success { "finished" } else { "failed" };
    push_activity(format!(
        "{} container {} {}: {} ({})",
        runtime_label(runtime),
        action,
        state,
        name,
        status
    ));
    let selected_matches = SELECTED_RUNTIME_CONTAINER
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|selected| selected.runtime == runtime && selected.name == name);
    if selected_matches && success && action == "delete" {
        *SELECTED_RUNTIME_CONTAINER.lock().unwrap() = None;
        *RUNTIME_CONTAINER_DETAILS.lock().unwrap() = None;
    } else if selected_matches {
        request_selected_runtime_container_details();
    }
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_container_details_loaded(
    runtime: &str,
    name: &str,
    info: Vec<String>,
    logs: Vec<String>,
    stats: Vec<String>,
    error: Option<String>,
) {
    let selected_matches = SELECTED_RUNTIME_CONTAINER
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|selected| selected.runtime == runtime && selected.name == name);
    if !selected_matches {
        return;
    }
    let clean_lines = |lines: Vec<String>| {
        lines
            .into_iter()
            .map(|line| clean_session_log_line(&line))
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
    };
    *RUNTIME_CONTAINER_DETAILS.lock().unwrap() = Some(RuntimeContainerDetails {
        runtime: runtime.to_string(),
        name: name.to_string(),
        info: clean_lines(info),
        logs: clean_lines(logs),
        stats: clean_lines(stats),
        error,
    });
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_container_terminal_opened(runtime: &str, name: &str) {
    push_activity(format!(
        "Opened a {} terminal for {}.",
        runtime_label(runtime),
        name
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_container_terminal_failed(runtime: &str, name: &str, error: &str) {
    push_activity(format!(
        "Could not open a {} terminal for {}: {}",
        runtime_label(runtime),
        name,
        error
    ));
    show_error_alert(&format!("Could not open container terminal: {}", error));
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_machine_terminal_opened(runtime: &str, name: &str) {
    push_activity(format!(
        "Opened a {} machine shell for {}.",
        runtime_label(runtime),
        name
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_machine_terminal_failed(runtime: &str, name: &str, error: &str) {
    push_activity(format!(
        "Could not open a {} machine shell for {}: {}",
        runtime_label(runtime),
        name,
        error
    ));
    show_error_alert(&format!("Could not open machine shell: {}", error));
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_system_action_started(runtime: &str, action: &str) {
    if matches!(action, "start" | "stop") {
        let status = if action == "start" {
            RuntimeStatus::Starting
        } else {
            RuntimeStatus::Stopping
        };
        let runtime_name = runtime_label(runtime);
        set_runtime_system_state(
            runtime,
            status,
            format!("{} runtime {} is in progress.", runtime_name, action),
        );
        let key = runtime_task_key(runtime);
        start_operation_task(
            &key,
            if action == "start" {
                "Start Runtime"
            } else {
                "Stop Runtime"
            },
            runtime_name,
            ["Run runtime command", "Refresh runtime health"],
        );
        update_operation_task_step(
            &key,
            "Run runtime command",
            TaskStepStatus::Running,
            Some(format!("Running runtime {} command.", action)),
        );
    }
    push_activity(format!(
        "{} system {} started",
        runtime_label(runtime),
        action
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_runtime_system_action_log(runtime: &str, action: &str, line: &str) {
    push_activity(format!(
        "{} system {}: {}",
        runtime_label(runtime),
        action,
        line
    ));
    unsafe {
        rebuild_window_throttled(Duration::from_millis(500));
    }
}

pub fn record_runtime_system_action_finished(
    runtime: &str,
    action: &str,
    success: bool,
    status: &str,
) {
    invalidate_ui_command_cache();
    if matches!(action, "start" | "stop") {
        set_runtime_system_state(
            runtime,
            if success {
                if action == "start" {
                    RuntimeStatus::Ready
                } else {
                    RuntimeStatus::Unavailable
                }
            } else {
                RuntimeStatus::Failed
            },
            status,
        );
        finish_operation_task(
            &runtime_task_key(runtime),
            if success {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            },
            status,
        );
    }
    push_activity(format!(
        "{} system {} {} ({})",
        runtime_label(runtime),
        action,
        if success { "finished" } else { "failed" },
        status
    ));
    unsafe {
        rebuild_window();
    }
}

pub fn record_stop_success(index: usize) {
    invalidate_ui_command_cache();
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let detail = format!("{} stopped.", name);
    set_session_state(index, "Stopped", detail.clone());
    finish_operation_task(&stop_task_key(index), TaskStatus::Completed, detail.clone());
    push_activity(detail);
    unsafe {
        rebuild_window();
    }
}

pub fn record_stop_failure(index: usize, error: &str) {
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let detail = format!("{} stop failed: {}", name, error);
    set_session_state(index, "Error", detail.clone());
    finish_operation_task(&stop_task_key(index), TaskStatus::Failed, detail.clone());
    push_activity(detail);
    unsafe {
        rebuild_window();
    }
}

pub fn record_terminal_opened(index: usize) {
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    push_activity(format!("Terminal opened for {}.", name));
    unsafe {
        rebuild_window();
    }
}

pub fn record_terminal_open_failed(index: usize, error: &str) {
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let detail = format!("Terminal failed for {}: {}", name, error);
    push_activity(detail.clone());
    set_session_state(index, "Error", detail);
    unsafe {
        rebuild_window();
    }
}

pub fn record_process_exit(index: usize, process: &str, status: &str) {
    invalidate_ui_command_cache();
    let sessions = container_sessions::load_sessions();
    let name = sessions
        .get(index)
        .map(|session| session.name.as_str())
        .unwrap_or("Unknown application");
    let detail = format!("{} exited because {} ended with {}.", name, process, status);
    set_session_state(index, "Exited", detail.clone());
    finish_operation_task(&stop_task_key(index), TaskStatus::Completed, detail.clone());
    push_activity(detail);
    unsafe {
        rebuild_window();
    }
}

unsafe fn show_add_session_dialog() {
    unsafe {
        let image = preferred_gui_image_reference();
        show_add_session_dialog_with_defaults(session_defaults_for_image("container", &image));
    }
}

unsafe fn show_new_image_session_dialog() {
    unsafe {
        let image = preferred_gui_image_reference();
        show_add_session_dialog_with_defaults(AddSessionDefaults {
            name: "Niri Desktop".into(),
            runtime: "container".into(),
            display: "auto".into(),
            profile: "niri".into(),
            image,
            command: "niri".into(),
            ..AddSessionDefaults::default()
        });
    }
}

unsafe fn show_add_session_dialog_with_defaults(defaults: AddSessionDefaults) {
    unsafe {
        show_session_dialog(defaults, None);
    }
}

unsafe fn show_edit_session_dialog(index: usize) {
    let sessions = container_sessions::load_sessions();
    let Some(session) = sessions.get(index).cloned() else {
        show_error_alert("Application profile no longer exists.");
        return;
    };
    unsafe {
        show_session_dialog(defaults_from_session(&session), Some((index, session)));
    }
}

unsafe fn duplicate_session_profile(index: usize) {
    let sessions = container_sessions::load_sessions();
    let Some(source) = sessions.get(index).cloned() else {
        show_error_alert("Application profile no longer exists.");
        return;
    };
    let mut duplicate = source.clone();
    duplicate.name = unique_duplicate_name(&sessions, &source.name);

    match container_sessions::append_session(&duplicate) {
        Ok(()) => {
            let sessions = container_sessions::load_sessions();
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            let index = sessions.len().checked_sub(1);
            *SELECTED_SESSION.lock().unwrap() = index;
            if let Some(index) = index {
                invalidate_profile_validation(index);
            }
            push_activity(format!(
                "Duplicated application profile: {}",
                duplicate.name
            ));
            unsafe {
                rebuild_window();
            }
        }
        Err(error) => {
            let message = format!("Failed to duplicate session: {}", error);
            push_activity(message.clone());
            show_error_alert(&message);
        }
    }
}

unsafe fn export_session_profile(index: usize) {
    let sessions = container_sessions::load_sessions();
    let Some(session) = sessions.get(index) else {
        show_error_alert("Application profile no longer exists.");
        return;
    };
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let panel = unsafe { NSSavePanel::savePanel(mtm) };
    let filename = format!("{}.toml", display_slot_slug(&session.name));
    unsafe {
        panel.setNameFieldStringValue(&NSString::from_str(&filename));
        let _: () = msg_send![&*panel, setTitle:
            &*NSString::from_str("Export Application Profile")];
        let _: () = msg_send![&*panel, setMessage:
            &*NSString::from_str("Export this saved launch configuration without images, volumes, or containers.")];
    }
    if unsafe { panel.runModal() } != NSModalResponseOK {
        return;
    }
    let Some(url) = (unsafe { panel.URL() }) else {
        show_error_alert("No export destination was selected.");
        return;
    };
    let Some(path) = (unsafe { url.path() }) else {
        show_error_alert("The export destination is not a local file.");
        return;
    };
    match std::fs::write(
        path.to_string(),
        container_sessions::session_to_toml(session),
    ) {
        Ok(()) => push_activity(format!("Exported application profile: {}", session.name)),
        Err(error) => show_error_alert(&format!("Failed to export profile: {}", error)),
    }
}

unsafe fn show_raw_session_profile(index: usize) {
    let sessions = container_sessions::load_sessions();
    let Some(session) = sessions.get(index) else {
        show_error_alert("Application profile no longer exists.");
        return;
    };
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let alert: Retained<NSAlert> = unsafe { msg_send_id![NSAlert::class(), new] };
    unsafe {
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str(&format!("Raw Configuration: {}", session.name))];
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str("This is the exact TOML block stored for this application profile.")];
    }
    let view: Retained<NSView> =
        unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, 560.0, 300.0)] };
    let field = add_label(
        &view,
        &container_sessions::session_to_toml(session),
        rect(0.0, 0.0, 560.0, 300.0),
        mtm,
        TextStyle::Mono,
    );
    unsafe { field.setSelectable(true) };
    unsafe {
        let _: () = msg_send![&*alert, setAccessoryView: &*view];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Close")];
        let _: () = msg_send![&*alert, layout];
        let _: isize = msg_send![&*alert, runModal];
    }
}

fn defaults_from_session(session: &ContainerSession) -> AddSessionDefaults {
    AddSessionDefaults {
        name: session.name.clone(),
        runtime: session.runtime.clone(),
        display: session.display.clone().unwrap_or_else(|| "auto".into()),
        presentation: session
            .presentation
            .clone()
            .unwrap_or_else(|| "desktop".into()),
        profile: session.profile.clone().unwrap_or_else(|| "niri".into()),
        image: session.image.clone(),
        command: session
            .command
            .clone()
            .or_else(|| session.app.clone())
            .unwrap_or_default(),
        mounts: session.mounts.join("; "),
        env: session.env.join("; "),
        audio: session.audio,
    }
}

fn unique_duplicate_name(sessions: &[ContainerSession], name: &str) -> String {
    let base = format!("{} Copy", name);
    if !sessions.iter().any(|session| session.name == base) {
        return base;
    }

    for suffix in 2..100 {
        let candidate = format!("{} {}", base, suffix);
        if !sessions.iter().any(|session| session.name == candidate) {
            return candidate;
        }
    }

    format!(
        "{} {}",
        base,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default()
    )
}

unsafe fn show_session_dialog(
    defaults: AddSessionDefaults,
    edit_target: Option<(usize, ContainerSession)>,
) {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let alert: Retained<NSAlert> = unsafe { msg_send_id![NSAlert::class(), new] };
    let is_edit = edit_target.is_some();
    unsafe {
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str(if is_edit { "Edit Application" } else { "New Application" })];
        let _: () = msg_send![&*alert, setInformativeText:
        &*NSString::from_str(if is_edit {
            "Update this Container Mode profile. Advanced socket and waypipe fields are preserved."
        } else {
            "Create a Container Mode profile backed by Apple Container, Docker, or OrbStack."
        })];
    }

    let view: Retained<NSView> =
        unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, 420.0, 414.0)] };
    add_label(
        &view,
        "Name",
        rect(0.0, 385.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let name_field = add_text_field(
        &view,
        rect(116.0, 380.0, 304.0, 26.0),
        "Niri Desktop",
        &defaults.name,
        mtm,
    );
    add_label(
        &view,
        "Runtime",
        rect(0.0, 345.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let runtime_field = add_text_field(
        &view,
        rect(116.0, 340.0, 304.0, 26.0),
        "container",
        if defaults.runtime.is_empty() {
            "container"
        } else {
            &defaults.runtime
        },
        mtm,
    );
    add_label(
        &view,
        "Display",
        rect(0.0, 305.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let display_field = add_text_field(
        &view,
        rect(116.0, 300.0, 304.0, 26.0),
        "auto",
        if defaults.display.is_empty() {
            "auto"
        } else {
            &defaults.display
        },
        mtm,
    );
    add_label(
        &view,
        "Presentation",
        rect(0.0, 265.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let presentation_popup = add_popup(
        &view,
        rect(116.0, 260.0, 304.0, 28.0),
        &["Desktop", "Rootless"],
        usize::from(defaults.presentation.eq_ignore_ascii_case("rootless")),
        mtm,
    );
    add_label(
        &view,
        "Profile",
        rect(0.0, 225.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let profile_field = add_text_field(
        &view,
        rect(116.0, 220.0, 304.0, 26.0),
        "niri / single-app / shell",
        if defaults.profile.is_empty() {
            "niri"
        } else {
            &defaults.profile
        },
        mtm,
    );
    add_label(
        &view,
        "Image / Source",
        rect(0.0, 185.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let image_field = add_text_field(
        &view,
        rect(116.0, 180.0, 304.0, 26.0),
        smoke_image_reference(),
        &defaults.image,
        mtm,
    );
    add_label(
        &view,
        "Command",
        rect(0.0, 145.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let command_field = add_text_field(
        &view,
        rect(116.0, 140.0, 304.0, 26.0),
        "niri",
        &defaults.command,
        mtm,
    );
    add_label(
        &view,
        "Mounts",
        rect(0.0, 105.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let mounts_field = add_text_field(
        &view,
        rect(116.0, 100.0, 304.0, 26.0),
        "separate multiple mounts with ;",
        &defaults.mounts,
        mtm,
    );
    add_label(
        &view,
        "Env",
        rect(0.0, 65.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let env_field = add_text_field(
        &view,
        rect(116.0, 60.0, 304.0, 26.0),
        "WAYLAND_DEBUG=1; RUST_LOG=info",
        &defaults.env,
        mtm,
    );
    add_label(
        &view,
        "Audio",
        rect(0.0, 25.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let audio_popup = add_popup(
        &view,
        rect(116.0, 20.0, 304.0, 28.0),
        &["Off", "Forward playback (Apple, experimental)"],
        usize::from(defaults.audio),
        mtm,
    );

    unsafe {
        let _: () = msg_send![&*alert, setAccessoryView: &*view];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str(if is_edit { "Save" } else { "Create" })];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let _: () = msg_send![&*alert, layout];
    }

    let response: isize = unsafe { msg_send![&*alert, runModal] };
    if response != 1000 {
        return;
    }

    let name = field_string(&name_field);
    let runtime = field_string(&runtime_field);
    let display = field_string(&display_field);
    let presentation = if popup_index(&presentation_popup) == 1 {
        "rootless"
    } else {
        "desktop"
    };
    let profile = field_string(&profile_field);
    let image = field_string(&image_field);
    let command = field_string(&command_field);
    let mounts = semicolon_list(&field_string(&mounts_field));
    let env = semicolon_list(&field_string(&env_field));
    let audio_requested = popup_index(&audio_popup) == 1;
    if image.is_empty() {
        show_error_alert("Enter an image reference before creating the session.");
        return;
    }

    let runtime = if runtime.is_empty() {
        "container".to_string()
    } else {
        runtime
    };
    if audio_requested && matches!(runtime.as_str(), "docker" | "orb" | "orbstack") {
        show_error_alert("Audio forwarding currently requires Apple Container.");
        return;
    }
    let profile = normalize_profile(&profile);
    let runtime_args = default_gui_runtime_args(&runtime, profile.as_deref());
    let mut session = ContainerSession {
        name: if name.is_empty() {
            default_session_name(&image)
        } else {
            name
        },
        image,
        runtime,
        display: Some(if display.is_empty() {
            "auto".into()
        } else {
            display
        }),
        presentation: Some(presentation.into()),
        profile,
        app: None,
        command: if command.is_empty() {
            None
        } else {
            Some(command)
        },
        socket: None,
        container_socket: None,
        waypipe_path: None,
        waypipe_compress: None,
        waypipe_threads: None,
        audio: audio_requested,
        runtime_args,
        mounts,
        env,
    };

    let result = if let Some((index, original)) = edit_target {
        session.app = original.app;
        session.socket = original.socket;
        session.container_socket = original.container_socket;
        session.waypipe_path = original.waypipe_path;
        session.waypipe_compress = original.waypipe_compress;
        session.waypipe_threads = original.waypipe_threads;
        session.runtime_args = original.runtime_args;
        container_sessions::replace_session(index, &session).map(|_| Some(index))
    } else {
        container_sessions::append_session(&session).map(|_| None)
    };

    match result {
        Ok(selected_index) => {
            let sessions = container_sessions::load_sessions();
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            let selected_index = selected_index.or_else(|| sessions.len().checked_sub(1));
            *SELECTED_SESSION.lock().unwrap() = selected_index;
            if let Some(index) = selected_index {
                invalidate_profile_validation(index);
            }
            push_activity(format!(
                "{} session: {} ({})",
                if is_edit { "Updated" } else { "Added" },
                session.name,
                session.image
            ));
            unsafe {
                rebuild_window();
            }
        }
        Err(error) => {
            let message = format!(
                "Failed to {} session: {}",
                if is_edit { "update" } else { "add" },
                error
            );
            push_activity(message.clone());
            show_error_alert(&message);
        }
    }
}

unsafe fn show_pull_image_dialog() {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let alert: Retained<NSAlert> = unsafe { msg_send_id![NSAlert::class(), new] };
    unsafe {
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str("Pull an Image")];
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str("Choose a registry and destination. Base images still need waypipe and a GUI command before they can open a Cocoa-Way session.")];
    }

    let view: Retained<NSView> =
        unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, 420.0, 252.0)] };
    add_label(
        &view,
        "Destination",
        rect(0.0, 222.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let runtime_popup = add_popup(
        &view,
        rect(116.0, 216.0, 304.0, 28.0),
        &["Apple Container", "Docker-compatible Context"],
        0,
        mtm,
    );
    add_label(
        &view,
        "Registry",
        rect(0.0, 182.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let source_popup = add_popup(
        &view,
        rect(116.0, 176.0, 304.0, 28.0),
        &[
            "Docker Hub",
            "GitHub Container Registry",
            "Quay",
            "Custom OCI reference",
        ],
        0,
        mtm,
    );
    add_label(
        &view,
        "Reference",
        rect(0.0, 142.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let image_field = add_text_field(
        &view,
        rect(116.0, 136.0, 304.0, 26.0),
        "library/ubuntu:24.04",
        "library/ubuntu:24.04",
        mtm,
    );
    add_label(
        &view,
        "Platform",
        rect(0.0, 102.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let platform_popup = add_popup(
        &view,
        rect(116.0, 96.0, 304.0, 28.0),
        &["Native architecture", "Linux arm64", "Linux amd64"],
        0,
        mtm,
    );
    add_label(
        &view,
        "Connection",
        rect(0.0, 62.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let scheme_popup = add_popup(
        &view,
        rect(116.0, 56.0, 304.0, 28.0),
        &["Runtime default", "HTTPS", "HTTP (insecure)"],
        0,
        mtm,
    );
    add_label(
        &view,
        "After pull",
        rect(0.0, 22.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let after_popup = add_popup(
        &view,
        rect(116.0, 16.0, 304.0, 28.0),
        &["Keep as a local image", "Create an application"],
        1,
        mtm,
    );

    unsafe {
        let _: () = msg_send![&*alert, setAccessoryView: &*view];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Pull")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let _: () = msg_send![&*alert, layout];
    }

    let response: isize = unsafe { msg_send![&*alert, runModal] };
    if response != 1000 {
        return;
    }

    let reference = field_string(&image_field);
    if reference.is_empty() {
        show_error_alert("Image reference is required.");
        return;
    }
    let runtime = if popup_index(&runtime_popup) == 1 {
        "docker"
    } else {
        "container"
    };
    let image = normalize_registry_reference(popup_index(&source_popup), &reference);
    let platform = match popup_index(&platform_popup) {
        1 => Some("linux/arm64".into()),
        2 => Some("linux/amd64".into()),
        _ => None,
    };
    let scheme = match popup_index(&scheme_popup) {
        1 => Some("https".into()),
        2 => Some("http".into()),
        _ => None,
    };
    if !allow_storage_growth("pull an image") {
        return;
    }

    send(CompositorMessage::PullContainerImage {
        runtime: runtime.into(),
        image,
        platform,
        scheme,
        configure_session: popup_index(&after_popup) == 1,
    });
}

unsafe fn show_registry_login_dialog() {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let alert: Retained<NSAlert> = unsafe { msg_send_id![NSAlert::class(), new] };
    unsafe {
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str("Registry Login")];
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str("Credentials are passed to Apple Container through standard input and are never added to process arguments or Cocoa-Way logs.")];
    }

    let view: Retained<NSView> =
        unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, 420.0, 174.0)] };
    add_label(
        &view,
        "Registry",
        rect(0.0, 144.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let server_field = add_text_field(
        &view,
        rect(116.0, 138.0, 304.0, 26.0),
        "ghcr.io",
        "ghcr.io",
        mtm,
    );
    add_label(
        &view,
        "Username",
        rect(0.0, 104.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let username_field = add_text_field(
        &view,
        rect(116.0, 98.0, 304.0, 26.0),
        "account name",
        "",
        mtm,
    );
    add_label(
        &view,
        "Token / password",
        rect(0.0, 64.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let password_field = add_secure_text_field(
        &view,
        rect(116.0, 58.0, 304.0, 26.0),
        "personal access token",
        mtm,
    );
    add_label(
        &view,
        "Connection",
        rect(0.0, 24.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let scheme_popup = add_popup(
        &view,
        rect(116.0, 18.0, 304.0, 28.0),
        &["Runtime default", "HTTPS", "HTTP (insecure)"],
        0,
        mtm,
    );

    unsafe {
        let _: () = msg_send![&*alert, setAccessoryView: &*view];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Login")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let _: () = msg_send![&*alert, layout];
    }
    let response: isize = unsafe { msg_send![&*alert, runModal] };
    if response != 1000 {
        return;
    }

    let server = field_string(&server_field);
    let username = field_string(&username_field);
    let password = field_string(&password_field);
    if server.is_empty() || username.is_empty() || password.is_empty() {
        show_error_alert("Registry, username, and token/password are required.");
        return;
    }
    let scheme = match popup_index(&scheme_popup) {
        1 => Some("https".into()),
        2 => Some("http".into()),
        _ => None,
    };
    send(CompositorMessage::LoginContainerRegistry {
        server,
        username,
        password,
        scheme,
    });
}

unsafe fn show_load_image_dialog() {
    if !allow_storage_growth("load an OCI archive") {
        return;
    }
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let panel = unsafe { NSOpenPanel::openPanel(mtm) };
    unsafe {
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(false);
        let _: () = msg_send![&*panel, setTitle:
            &*NSString::from_str("Load OCI Image Archive")];
        let _: () = msg_send![&*panel, setMessage:
            &*NSString::from_str("Choose an OCI-compatible image tar archive to load into Apple Container.")];
    }

    let response = unsafe { panel.runModal() };
    if response != NSModalResponseOK {
        return;
    }

    let Some(url) = (unsafe { panel.URL() }) else {
        show_error_alert("No archive was selected.");
        return;
    };
    let Some(path) = (unsafe { url.path() }) else {
        show_error_alert("Selected archive does not have a local filesystem path.");
        return;
    };

    send(CompositorMessage::LoadContainerImage {
        path: path.to_string(),
    });
}

unsafe fn show_delete_image_dialog() {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let alert: Retained<NSAlert> = unsafe { msg_send_id![NSAlert::class(), new] };
    unsafe {
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str("Delete Image")];
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str("Delete a local image from Apple Container or Docker. Running containers are not stopped by this action.")];
    }

    let view: Retained<NSView> =
        unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, 380.0, 94.0)] };
    add_label(
        &view,
        "Runtime",
        rect(0.0, 65.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let runtime_field = add_text_field(
        &view,
        rect(116.0, 60.0, 264.0, 26.0),
        "container",
        "container",
        mtm,
    );
    add_label(
        &view,
        "Image",
        rect(0.0, 25.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let image_field = add_text_field(
        &view,
        rect(116.0, 20.0, 264.0, 26.0),
        smoke_image_reference(),
        "",
        mtm,
    );

    unsafe {
        let _: () = msg_send![&*alert, setAccessoryView: &*view];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Delete")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let _: () = msg_send![&*alert, layout];
    }

    let response: isize = unsafe { msg_send![&*alert, runModal] };
    if response != 1000 {
        return;
    }

    let runtime = field_string(&runtime_field);
    let image = field_string(&image_field);
    if image.is_empty() {
        show_error_alert("Image is required.");
        return;
    }

    send(CompositorMessage::DeleteContainerImage {
        runtime: if runtime.is_empty() {
            "container".into()
        } else {
            runtime
        },
        image,
    });
}

unsafe fn show_create_volume_dialog() {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let alert: Retained<NSAlert> = unsafe { msg_send_id![NSAlert::class(), new] };
    unsafe {
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str("Create Volume")];
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str("Choose the runtime that will own this persistent volume.")];
    }

    let view: Retained<NSView> =
        unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, 380.0, 134.0)] };
    add_label(
        &view,
        "Name",
        rect(0.0, 105.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let volume_field = add_text_field(
        &view,
        rect(116.0, 100.0, 264.0, 26.0),
        "project-data",
        "",
        mtm,
    );
    add_label(
        &view,
        "Runtime",
        rect(0.0, 65.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let runtime_popup = add_popup(
        &view,
        rect(116.0, 60.0, 264.0, 26.0),
        &["Apple Container", "Docker-compatible Context"],
        0,
        mtm,
    );
    add_label(
        &view,
        "Type",
        rect(0.0, 25.0, 120.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        &view,
        "Managed volume",
        rect(116.0, 22.0, 264.0, 20.0),
        mtm,
        TextStyle::Body,
    );

    unsafe {
        let _: () = msg_send![&*alert, setAccessoryView: &*view];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Create")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let _: () = msg_send![&*alert, layout];
    }

    let response: isize = unsafe { msg_send![&*alert, runModal] };
    if response != 1000 {
        return;
    }

    let runtime_index: isize = unsafe { msg_send![&*runtime_popup, indexOfSelectedItem] };
    let volume = field_string(&volume_field);
    if volume.is_empty() {
        show_error_alert("Volume name is required.");
        return;
    }
    send(CompositorMessage::CreateContainerVolume {
        runtime: if runtime_index == 1 {
            "docker".into()
        } else {
            "container".into()
        },
        volume,
    });
}

unsafe fn delete_container_session(index: usize) {
    let sessions = container_sessions::load_sessions();
    let Some(session) = sessions.get(index) else {
        show_error_alert("Application profile no longer exists.");
        return;
    };
    if active_session(index).is_some() || session_can_stop(session_state(index).as_ref()) {
        show_error_alert("Stop all running instances before deleting this profile.");
        return;
    }
    if !confirm_delete_session(&session.name) {
        return;
    }

    match container_sessions::remove_session(index) {
        Ok(()) => {
            *SELECTED_SESSION.lock().unwrap() = None;
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            invalidate_all_profile_validation();
            push_activity(format!("Deleted application profile: {}", session.name));
            unsafe {
                rebuild_window();
            }
        }
        Err(error) => {
            let message = format!("Failed to delete session: {}", error);
            push_activity(message.clone());
            show_error_alert(&message);
        }
    }
}

fn confirm_delete_session(name: &str) -> bool {
    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str(&format!("Delete profile “{}”?", name))];
        let message = format!(
            "This removes the saved launch configuration for '{}'. Images, volumes, containers, and displays will not be deleted.",
            name,
        );
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str(&message)];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Delete")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let response: isize = msg_send![&*alert, runModal];
        response == 1000
    }
}

fn confirm_delete_resource(kind: &str, runtime: &str, name: &str) -> bool {
    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let title = format!("Delete {}?", kind);
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str(&title)];
        let message = format!(
            "This deletes '{}' from {}. Running containers that use it may fail.",
            name,
            runtime_label(runtime)
        );
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str(&message)];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Delete")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let response: isize = msg_send![&*alert, runModal];
        response == 1000
    }
}

fn confirm_image_removal(action: &ImageDeleteAction, remove_tag: bool) -> bool {
    let sessions = container_sessions::load_sessions();
    let known_references = if remove_tag {
        vec![action.reference.clone()]
    } else {
        action
            .image_id
            .as_deref()
            .map(|image_id| image_references_for_id(&action.runtime, image_id))
            .filter(|references| !references.is_empty())
            .unwrap_or_else(|| vec![action.reference.clone()])
    };
    let referenced = sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| {
            known_references.contains(&session.image)
                && runtime_key_matches(&action.runtime, &session.runtime)
        })
        .collect::<Vec<_>>();
    let running = referenced
        .iter()
        .filter(|(index, _)| active_session(*index).is_some())
        .map(|(_, session)| session.name.as_str())
        .collect::<Vec<_>>();
    if !running.is_empty() {
        show_error_alert(&format!(
            "This image cannot be removed while it is used by running applications:\n\n{}\n\nStop those instances and try again.",
            running
                .iter()
                .map(|name| format!("• {}", name))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        return false;
    }

    let referenced_names = referenced
        .iter()
        .map(|(_, session)| session.name.as_str())
        .collect::<Vec<_>>();
    let tags = if remove_tag {
        Vec::new()
    } else {
        known_references
    };
    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let title = if remove_tag {
            format!("Remove tag “{}”?", action.reference)
        } else {
            "Delete image and all local data?".into()
        };
        let _: () = msg_send![&*alert, setMessageText: &*NSString::from_str(&title)];
        let mut details = vec![format!("Runtime: {}", runtime_label(&action.runtime))];
        if remove_tag {
            details.push("Only this repository tag will be removed.".into());
        } else if !tags.is_empty() {
            details.push(format!("Known tags:\n{}", tags.join("\n")));
        }
        if !referenced_names.is_empty() {
            details.push(format!(
                "Referenced by saved profiles:\n{}",
                referenced_names
                    .iter()
                    .map(|name| format!("• {}", name))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str(&details.join("\n\n"))];
        let action_title = if remove_tag {
            "Remove Tag"
        } else {
            "Delete Image"
        };
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str(action_title)];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let response: isize = msg_send![&*alert, runModal];
        response == 1000
    }
}

fn confirm_volume_removal(action: &VolumeDeleteAction) -> bool {
    let usage = volume_usage(&action.runtime, &action.name);
    if usage.loading {
        show_error_alert(
            "Volume usage is still loading. Wait a moment, press Reload, and try again.",
        );
        return false;
    }
    if let Some(error) = usage.error {
        show_error_alert(&format!(
            "Cocoa-Way could not verify whether this volume is mounted, so deletion was blocked.\n\n{}",
            error
        ));
        return false;
    }
    if !usage.mounted_containers.is_empty() {
        show_error_alert(&format!(
            "This volume is mounted by:\n\n{}\n\nStop those containers before deleting the volume.",
            usage
                .mounted_containers
                .iter()
                .map(|name| format!("• {}", name))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        return false;
    }

    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str(&format!("Delete volume “{}”?", action.name))];
        let mut details = vec![format!("Runtime: {}", runtime_label(&action.runtime))];
        if !usage.referenced_profiles.is_empty() {
            details.push(format!(
                "Referenced by saved application profiles:\n{}\n\nThose profiles will remain saved, but their next launch may fail until the volume is recreated.",
                usage
                    .referenced_profiles
                    .iter()
                    .map(|name| format!("• {}", name))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str(&details.join("\n\n"))];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Delete Volume")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let response: isize = msg_send![&*alert, runModal];
        response == 1000
    }
}

fn confirm_close_managed_display(slot: &str) -> bool {
    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str("Close Managed Display?")];
        let message = format!(
            "Closing '{}' disconnects GUI clients attached through copied environment variables, including clients Cocoa-Way cannot track.",
            slot
        );
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str(&message)];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Close Display")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let response: isize = msg_send![&*alert, runModal];
        response == 1000
    }
}

fn confirm_stop_apple_runtime() -> Option<Vec<usize>> {
    let sessions = container_sessions::load_sessions();
    let mut running = active_sessions_snapshot()
        .into_iter()
        .filter_map(|active| {
            let session = sessions.get(active.instance.profile_index)?;
            if normalized_runtime_key(&session.runtime) != "apple" {
                return None;
            }
            Some((active.instance.profile_index, session.name.clone()))
        })
        .collect::<Vec<_>>();
    running.sort_by_key(|(index, _)| *index);
    running.dedup_by_key(|(index, _)| *index);
    if running.is_empty() {
        return Some(Vec::new());
    }

    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let _: () = msg_send![&*alert, setMessageText:
        &*NSString::from_str(&format!(
            "Apple Container has {} running Cocoa-Way instance{}.",
            running.len(),
            if running.len() == 1 { "" } else { "s" }
        ))];
        let list = running
            .iter()
            .map(|(_, name)| format!("• {name}"))
            .collect::<Vec<_>>()
            .join("\n");
        let detail = format!(
            "Stopping the runtime requires these application instances to be terminated first:\n\n{list}\n\nThis does not delete their saved profiles or images."
        );
        let _: () = msg_send![&*alert, setInformativeText: &*NSString::from_str(&detail)];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Stop Instances and Runtime")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Cancel")];
        let response: isize = msg_send![&*alert, runModal];
        if response == 1000 {
            Some(running.into_iter().map(|(index, _)| index).collect())
        } else {
            None
        }
    }
}

fn show_container_settings_dialog() {
    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str("Cocoa-Way Settings")];
        let config_path = container_sessions::config_path();
        let detail = format!(
            "General\nLocal, open-source control plane.\n\nRuntime\nApple Container is first-class; Docker-compatible contexts are optional providers.\n\nDisplay\nProfiles may use auto, default, or a named managed display.\n\nStorage\nImage and volume operations are checked before destructive actions.\n\nAdvanced\nConfiguration file: {}",
            config_path.display()
        );
        let _: () = msg_send![&*alert, setInformativeText: &*NSString::from_str(&detail)];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Open Configuration File")];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("Done")];
        let response: isize = msg_send![&*alert, runModal];
        if response == 1000 {
            let _ = Command::new("open").arg("-R").arg(config_path).spawn();
        }
    }
}

fn default_session_name(image: &str) -> String {
    if image.to_ascii_lowercase().contains("niri") {
        return "Niri Desktop".into();
    }

    image
        .rsplit('/')
        .next()
        .and_then(|tail| tail.split(':').next())
        .filter(|value| !value.is_empty())
        .unwrap_or("Application")
        .replace('-', " ")
}

fn session_defaults_for_image(runtime: &str, image: &str) -> AddSessionDefaults {
    let is_niri_image = image.to_ascii_lowercase().contains("niri");
    AddSessionDefaults {
        name: default_session_name(image),
        runtime: runtime.into(),
        display: "auto".into(),
        profile: if is_niri_image {
            "niri".into()
        } else {
            "single-app".into()
        },
        image: image.into(),
        command: if is_niri_image {
            "niri".into()
        } else {
            String::new()
        },
        ..AddSessionDefaults::default()
    }
}

unsafe fn show_session_dialog_for_image(runtime: &str, image: &str) {
    unsafe {
        show_add_session_dialog_with_defaults(session_defaults_for_image(runtime, image));
    }
}

fn normalize_registry_reference(source: isize, reference: &str) -> String {
    let reference = reference.trim().trim_start_matches("docker://");
    let first = reference.split('/').next().unwrap_or_default();
    let has_registry = reference.contains('/')
        && (first.contains('.') || first.contains(':') || first == "localhost");
    if has_registry || source == 3 {
        return reference.to_string();
    }

    match source {
        1 => format!("ghcr.io/{}", reference.trim_start_matches('/')),
        2 => format!("quay.io/{}", reference.trim_start_matches('/')),
        _ if reference.contains('/') => {
            format!("docker.io/{}", reference.trim_start_matches('/'))
        }
        _ => format!("docker.io/library/{}", reference),
    }
}

fn field_string(field: &NSTextField) -> String {
    let value: Retained<NSString> = unsafe { msg_send_id![field, stringValue] };
    value.to_string().trim().to_string()
}

fn popup_index(popup: &NSPopUpButton) -> isize {
    unsafe { msg_send![popup, indexOfSelectedItem] }
}

fn semicolon_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn show_error_alert(message: &str) {
    unsafe {
        let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
        let _: () = msg_send![&*alert, setMessageText:
            &*NSString::from_str("Container Mode")];
        let _: () = msg_send![&*alert, setInformativeText:
            &*NSString::from_str(message)];
        let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
            &*NSString::from_str("OK")];
        let _: isize = msg_send![&*alert, runModal];
    }
}

fn remember_stop_request(index: usize) {
    let sessions = container_sessions::load_sessions();
    let message = match sessions.get(index) {
        Some(session) => {
            start_operation_task(
                stop_task_key(index),
                "Stop instance",
                &session.name,
                [
                    "Ask application to exit",
                    "Stop application process",
                    "Stop Waypipe worker",
                    "Release display",
                    "Stop container",
                    "Mark instance exited",
                ],
            );
            set_session_state(index, "Stopping", format!("Stopping {}.", session.name));
            format!("Stop requested: {}", session.name)
        }
        None => format!(
            "Stop requested for missing application profile #{}",
            index + 1
        ),
    };
    push_activity(message);
}

declare_class!(
    pub struct ContainerModeHandler;

    unsafe impl ClassType for ContainerModeHandler {
        type Super = NSObject;
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "CocoaWayContainerModeHandler";
    }

    impl DeclaredClass for ContainerModeHandler {
        type Ivars = ();
    }

    unsafe impl ContainerModeHandler {
        #[method(windowShouldClose:)]
        fn window_should_close(&self, window: &AnyObject) -> bool {
            unsafe {
                let _: () = msg_send![window, orderOut: None::<&AnyObject>];
            }
            false
        }

        #[method(windowDidResize:)]
        fn window_did_resize(&self, _notification: &AnyObject) {
            unsafe {
                refresh_window_for_resize(Duration::from_millis(16));
            }
        }

        #[method(windowDidEndLiveResize:)]
        fn window_did_end_live_resize(&self, _notification: &AnyObject) {
            *LAST_RESIZE_REBUILD.lock().unwrap() = None;
            unsafe {
                refresh_window_without_focus();
            }
        }

        #[method(launchContainerSession:)]
        fn launch_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            if let Some(conflict) = active_display_conflict(index) {
                let message = format!(
                    "Default display is already used by '{}'. Stop that session before launching another one.",
                    conflict
                );
                set_session_state(index, "Blocked", message.clone());
                push_activity(format!("Launch blocked: {}", message));
                *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
                *SELECTED_SESSION.lock().unwrap() = Some(index);
                show_error_alert(&message);
                unsafe { rebuild_window(); }
                return;
            }
            remember_launch_request(index);
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            *SELECTED_SESSION.lock().unwrap() = Some(index);
            send(CompositorMessage::StartContainerSession(index));
            unsafe { rebuild_window(); }
        }

        #[method(checkContainerSession:)]
        fn check_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            *SELECTED_SESSION.lock().unwrap() = Some(index);
            set_session_state(index, "Checking", "Running launch preflight checks.".into());
            push_activity(format!("Check requested for session #{}", index + 1));
            send(CompositorMessage::CheckContainerSession(index));
            unsafe { rebuild_window(); }
        }

        #[method(stopContainerSession:)]
        fn stop_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            if !session_can_stop(session_state(index).as_ref()) {
                let name = container_sessions::load_sessions()
                    .get(index)
                    .map(|session| session.name.clone())
                    .unwrap_or_else(|| format!("session #{}", index + 1));
                push_activity(format!("Stop ignored: {} is not running.", name));
                *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
                *SELECTED_SESSION.lock().unwrap() = Some(index);
                unsafe { rebuild_window(); }
                return;
            }
            remember_stop_request(index);
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            *SELECTED_SESSION.lock().unwrap() = Some(index);
            send(CompositorMessage::StopContainerSession(index));
            unsafe { rebuild_window(); }
        }

        #[method(forceStopContainerSession:)]
        fn force_stop_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            push_activity(format!("Force Stop requested for application #{}", index + 1));
            send(CompositorMessage::ForceStopContainerSession(index));
            unsafe { rebuild_window(); }
        }

        #[method(openContainerTerminal:)]
        fn open_container_terminal(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            *SELECTED_SESSION.lock().unwrap() = Some(index);
            *SELECTED_TAB.lock().unwrap() = 2;
            push_activity(format!("Terminal requested for session #{}", index + 1));
            send(CompositorMessage::OpenContainerTerminal(index));
            unsafe { rebuild_window(); }
        }

        #[method(copyApplicationDiagnostics:)]
        fn copy_application_diagnostics(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            let sessions = container_sessions::load_sessions();
            let Some(session) = sessions.get(index) else {
                show_error_alert("Application profile no longer exists.");
                return;
            };
            let state = session_state(index);
            let active = active_session(index);
            let logs = control_session_logs(index, 80);
            let (status, detail) = state
                .as_ref()
                .map(|state| (session_state_label(state), state.detail.as_str()))
                .unwrap_or(("Not checked", "No runtime status has been recorded."));
            let active_detail = active
                .map(|snapshot| {
                    format!(
                        "instance={} display={} waypipe_pid={} container_pid={}",
                        snapshot.instance.id,
                        snapshot.instance.display_slot,
                        snapshot.instance.waypipe_pid,
                        snapshot
                            .instance
                            .container_pid
                            .map(|pid| pid.to_string())
                            .unwrap_or_else(|| "unknown".into())
                    )
                })
                .unwrap_or_else(|| "none".into());
            let recent_logs = if logs.is_empty() {
                "No captured logs.".into()
            } else {
                logs.join("\n")
            };
            let diagnostics = format!(
                "Cocoa-Way application diagnostics\n\nApplication: {}\nRuntime: {}\nImage: {}\nCommand: {}\nDisplay: {}\nPresentation: {}\nStatus: {}\nStatus detail: {}\nActive instance: {}\n\nRecent logs:\n{}",
                session.name,
                runtime_label(&session.runtime),
                session.image,
                session_display_command(session),
                session_display_summary(session),
                session_presentation_summary(session),
                status,
                detail,
                active_detail,
                recent_logs,
            );
            unsafe {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(
                    &NSString::from_str(&diagnostics),
                    NSPasteboardTypeString,
                );
            }
            push_activity(format!("Copied diagnostics for application: {}", session.name));
        }

        #[method(reloadContainerMode:)]
        fn reload_container_mode(&self, _sender: &AnyObject) {
            invalidate_ui_command_cache();
            request_selected_runtime_container_details();
            unsafe { rebuild_window(); }
        }

        #[method(createManagedDisplay:)]
        fn create_managed_display(&self, _sender: &AnyObject) {
            send(CompositorMessage::CreateManagedDisplay);
        }

        #[method(copyManagedDisplayEnvironment:)]
        fn copy_managed_display_environment(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let display = MANAGED_DISPLAY_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(display) = display else {
                show_error_alert("Managed display no longer exists.");
                return;
            };
            let command = format!(
                "export XDG_RUNTIME_DIR={} WAYLAND_DISPLAY={}",
                shell_single_quote(&display.runtime_dir),
                shell_single_quote(&display.display)
            );
            unsafe {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(
                    &NSString::from_str(&command),
                    NSPasteboardTypeString,
                );
            }
            push_activity(format!("Copied environment for managed display: {}", display.slot));
        }

        #[method(copyManagedDisplayCommand:)]
        fn copy_managed_display_command(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let display = MANAGED_DISPLAY_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(display) = display else {
                show_error_alert("Managed display no longer exists.");
                return;
            };
            let command = format!(
                "./run_waypipe.sh --display {}",
                shell_single_quote(&display.slot)
            );
            unsafe {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(
                    &NSString::from_str(&command),
                    NSPasteboardTypeString,
                );
            }
            push_activity(format!(
                "Copied run_waypipe.sh command for managed display: {}",
                display.slot
            ));
        }

        #[method(focusManagedDisplay:)]
        fn focus_managed_display(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let display = MANAGED_DISPLAY_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(display) = display else {
                show_error_alert("Managed display no longer exists.");
                return;
            };
            let application = unsafe {
                NSRunningApplication::runningApplicationWithProcessIdentifier(
                    display.pid as libc::pid_t,
                )
            };
            let Some(application) = application else {
                show_error_alert("The managed display process is no longer running.");
                return;
            };
            let activated = unsafe {
                application.activateWithOptions(
                    NSApplicationActivationOptions::NSApplicationActivateAllWindows,
                )
            };
            if !activated {
                show_error_alert("macOS could not focus the managed display window.");
            }
        }

        #[method(closeManagedDisplay:)]
        fn close_managed_display(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let display = MANAGED_DISPLAY_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(display) = display else {
                show_error_alert("Managed display no longer exists.");
                return;
            };
            if confirm_close_managed_display(&display.slot) {
                CLOSING_MANAGED_DISPLAYS
                    .lock()
                    .unwrap()
                    .push(display.slot.clone());
                send(CompositorMessage::CloseManagedDisplay(display.slot));
                unsafe { rebuild_window(); }
            }
        }

        #[method(releaseDisplayAttachment:)]
        fn release_display_attachment(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            if !session_can_stop(session_state(index).as_ref()) {
                push_activity(format!(
                    "Display release ignored: application #{} is not running.",
                    index + 1
                ));
                unsafe { rebuild_window(); }
                return;
            }
            remember_stop_request(index);
            send(CompositorMessage::StopContainerSession(index));
            unsafe { rebuild_window(); }
        }

        #[method(clearContainerActivity:)]
        fn clear_container_activity(&self, _sender: &AnyObject) {
            ACTIVITY.lock().unwrap().clear();
            unsafe { rebuild_window(); }
        }

        #[method(openContainerConfig:)]
        fn open_container_config(&self, _sender: &AnyObject) {
            let path = container_sessions::config_path();
            let _ = container_sessions::load_sessions();
            let _ = std::process::Command::new("open")
                .arg("-R")
                .arg(path)
                .spawn();
        }

        #[method(openContainerSettings:)]
        fn open_container_settings(&self, _sender: &AnyObject) {
            show_container_settings_dialog();
        }

        #[method(addContainerSession:)]
        fn add_container_session(&self, _sender: &AnyObject) {
            unsafe { show_add_session_dialog(); }
        }

        #[method(editContainerSession:)]
        fn edit_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            unsafe { show_edit_session_dialog(tag.max(0) as usize); }
        }

        #[method(changeContainerSessionPresentation:)]
        fn change_container_session_presentation(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            if active_session(index).is_some() {
                show_error_alert("Stop this application before changing its presentation mode.");
                unsafe { rebuild_window(); }
                return;
            }
            let selected: isize = unsafe { msg_send![sender, indexOfSelectedItem] };
            let sessions = container_sessions::load_sessions();
            let Some(mut session) = sessions.get(index).cloned() else {
                show_error_alert("Application profile no longer exists.");
                unsafe { rebuild_window(); }
                return;
            };
            let presentation = if selected == 1 { "rootless" } else { "desktop" };
            session.presentation = Some(presentation.into());
            match container_sessions::replace_session(index, &session) {
                Ok(()) => {
                    invalidate_profile_validation(index);
                    push_activity(format!(
                        "{} presentation changed to {}.",
                        session.name,
                        session_presentation_summary(&session)
                    ));
                }
                Err(error) => {
                    let message = format!("Failed to update presentation mode: {}", error);
                    push_activity(message.clone());
                    show_error_alert(&message);
                }
            }
            unsafe { rebuild_window(); }
        }

        #[method(duplicateContainerSession:)]
        fn duplicate_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            unsafe { duplicate_session_profile(tag.max(0) as usize); }
        }

        #[method(applicationProfileMoreAction:)]
        fn application_profile_more_action(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let selected: isize = unsafe { msg_send![sender, indexOfSelectedItem] };
            let index = tag.max(0) as usize;
            match selected {
                1 => unsafe { duplicate_session_profile(index) },
                2 => unsafe { export_session_profile(index) },
                3 => unsafe { show_raw_session_profile(index) },
                4 => unsafe { delete_container_session(index) },
                _ => {}
            }
            unsafe { rebuild_window(); }
        }

        #[method(viewRawContainerSession:)]
        fn view_raw_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            unsafe { show_raw_session_profile(tag.max(0) as usize); }
        }

        #[method(newImageContainerSession:)]
        fn new_image_container_session(&self, _sender: &AnyObject) {
            unsafe { show_new_image_session_dialog(); }
        }

        #[method(restoreSmokeContainerSession:)]
        fn restore_smoke_container_session(&self, _sender: &AnyObject) {
            add_or_select_smoke_session();
            unsafe { rebuild_window(); }
        }

        #[method(pullContainerImage:)]
        fn pull_container_image(&self, _sender: &AnyObject) {
            unsafe { show_pull_image_dialog(); }
        }

        #[method(loginContainerRegistry:)]
        fn login_container_registry(&self, _sender: &AnyObject) {
            unsafe { show_registry_login_dialog(); }
        }

        #[method(loadContainerImage:)]
        fn load_container_image(&self, _sender: &AnyObject) {
            unsafe { show_load_image_dialog(); }
        }

        #[method(buildSmokeContainerImage:)]
        fn build_smoke_container_image(&self, _sender: &AnyObject) {
            request_smoke_image_build();
            unsafe { rebuild_window(); }
        }

        #[method(buildSmokeContainerSessionImage:)]
        fn build_smoke_container_session_image(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            *SELECTED_SESSION.lock().unwrap() = Some(tag.max(0) as usize);
            request_smoke_image_build();
            unsafe { rebuild_window(); }
        }

        #[method(startAppleContainerSystem:)]
        fn start_apple_container_system(&self, _sender: &AnyObject) {
            send(CompositorMessage::StartAppleContainerSystem);
            unsafe { rebuild_window(); }
        }

        #[method(stopAppleContainerSystem:)]
        fn stop_apple_container_system(&self, _sender: &AnyObject) {
            let Some(running_profiles) = confirm_stop_apple_runtime() else {
                return;
            };
            for index in running_profiles {
                remember_stop_request(index);
                send(CompositorMessage::ForceStopContainerSession(index));
            }
            send(CompositorMessage::RuntimeSystemAction {
                runtime: "apple".into(),
                action: "stop".into(),
            });
            unsafe { rebuild_window(); }
        }

        #[method(pullContainerSessionImage:)]
        fn pull_container_session_image(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            let sessions = container_sessions::load_sessions();
            let Some(session) = sessions.get(index) else {
                show_error_alert("Application profile no longer exists.");
                return;
            };
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            *SELECTED_SESSION.lock().unwrap() = Some(index);
            push_activity(format!(
                "Pull requested for missing image: {}",
                session.image
            ));
            if !allow_storage_growth("pull the missing session image") {
                return;
            }
            send(CompositorMessage::PullContainerImage {
                runtime: session.runtime.clone(),
                image: session.image.clone(),
                platform: None,
                scheme: None,
                configure_session: false,
            });
            unsafe { rebuild_window(); }
        }

        #[method(loadContainerSessionImage:)]
        fn load_container_session_image(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            *SELECTED_SESSION.lock().unwrap() = Some(tag.max(0) as usize);
            unsafe { show_load_image_dialog(); }
        }

        #[method(copySmokeImageBuildCommand:)]
        fn copy_smoke_image_build_command(&self, _sender: &AnyObject) {
            unsafe {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(
            &NSString::from_str(&smoke_image_build_command()),
            NSPasteboardTypeString,
        );
    }
    push_activity("Copied example image build command.".into());
            unsafe { rebuild_window(); }
        }

        #[method(deleteContainerImage:)]
        fn delete_container_image(&self, _sender: &AnyObject) {
            unsafe { show_delete_image_dialog(); }
        }

        #[method(createContainerVolume:)]
        fn create_container_volume(&self, _sender: &AnyObject) {
            unsafe { show_create_volume_dialog(); }
        }

        #[method(createContainerSessionFromImage:)]
        fn create_container_session_from_image(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = IMAGE_CREATE_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some((runtime, image)) = action else {
                show_error_alert("Image action no longer exists. Press Reload and try again.");
                return;
            };
            unsafe {
                show_session_dialog_for_image(&runtime, &image);
            }
        }

        #[method(deleteLocalContainerImage:)]
        fn delete_local_container_image(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = IMAGE_DELETE_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(action) = action else {
                show_error_alert("Image action no longer exists. Press Reload and try again.");
                return;
            };
            if !confirm_image_removal(&action, false) {
                return;
            }
            send(CompositorMessage::DeleteContainerImage {
                runtime: action.runtime,
                image: action.reference,
            });
        }

        #[method(imageMoreAction:)]
        fn image_more_action(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let selected: isize = unsafe { msg_send![sender, indexOfSelectedItem] };
            let action = IMAGE_DELETE_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(mut action) = action else {
                show_error_alert("Image action no longer exists. Press Reload and try again.");
                return;
            };
            let remove_tag = selected == 1 && image_reference_has_tag(&action.reference);
            if selected == 0 || !confirm_image_removal(&action, remove_tag) {
                return;
            }
            if !remove_tag {
                if let Some(image_id) = action.image_id.take() {
                    action.reference = image_id;
                }
            }
            send(CompositorMessage::DeleteContainerImage {
                runtime: action.runtime,
                image: action.reference,
            });
        }

        #[method(deleteLocalContainerVolume:)]
        fn delete_local_container_volume(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = VOLUME_DELETE_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(action) = action else {
                show_error_alert("Volume action no longer exists. Press Reload and try again.");
                return;
            };
            if !confirm_volume_removal(&action) {
                return;
            }
            send(CompositorMessage::DeleteContainerVolume {
                runtime: action.runtime,
                volume: action.name,
            });
        }

        #[method(stopRuntimeContainer:)]
        fn stop_runtime_container(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = RUNTIME_CONTAINER_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some((runtime, name)) = action else {
                show_error_alert("Container action no longer exists. Press Reload and try again.");
                return;
            };
            send(CompositorMessage::StopRuntimeContainer { runtime, name });
        }

        #[method(startRuntimeContainer:)]
        fn start_runtime_container(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = RUNTIME_CONTAINER_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some((runtime, name)) = action else {
                show_error_alert("Container action no longer exists. Press Reload and try again.");
                return;
            };
            send(CompositorMessage::StartRuntimeContainer { runtime, name });
        }

        #[method(deleteRuntimeContainer:)]
        fn delete_runtime_container(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = RUNTIME_CONTAINER_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some((runtime, name)) = action else {
                show_error_alert("Container action no longer exists. Press Reload and try again.");
                return;
            };
            if !confirm_delete_resource("Container", &runtime, &name) {
                return;
            }
            send(CompositorMessage::DeleteRuntimeContainer { runtime, name });
        }

        #[method(restartRuntimeContainer:)]
        fn restart_runtime_container(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = RUNTIME_CONTAINER_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some((runtime, name)) = action else {
                show_error_alert("Container action no longer exists. Press Reload and try again.");
                return;
            };
            send(CompositorMessage::RestartRuntimeContainer { runtime, name });
        }

        #[method(openRuntimeContainerTerminal:)]
        fn open_runtime_container_terminal(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let action = RUNTIME_CONTAINER_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some((runtime, name)) = action else {
                show_error_alert("Container action no longer exists. Press Reload and try again.");
                return;
            };
            send(CompositorMessage::OpenRuntimeContainerTerminal { runtime, name });
        }

        #[method(selectRuntimeContainer:)]
        fn select_runtime_container(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let selected = RUNTIME_CONTAINER_SELECT_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(selected) = selected else {
                show_error_alert("Container selection no longer exists. Press Reload and try again.");
                return;
            };
            *SELECTED_NAV.lock().unwrap() = runtime_nav(&selected.runtime);
            *SELECTED_RUNTIME_CONTAINER.lock().unwrap() = Some(selected.clone());
            *RUNTIME_CONTAINER_DETAILS.lock().unwrap() = None;
            send(CompositorMessage::RefreshRuntimeContainerDetails {
                runtime: selected.runtime,
                name: selected.name,
            });
            unsafe { rebuild_window(); }
        }

        #[method(refreshRuntimeContainerDetails:)]
        fn refresh_runtime_container_details(&self, _sender: &AnyObject) {
            request_selected_runtime_container_details();
            unsafe { rebuild_window(); }
        }

        #[method(startOrbStack:)]
        fn start_orbstack(&self, _sender: &AnyObject) {
            send(CompositorMessage::RuntimeSystemAction {
                runtime: "orbstack".into(),
                action: "start".into(),
            });
        }

        #[method(stopOrbStack:)]
        fn stop_orbstack(&self, _sender: &AnyObject) {
            send(CompositorMessage::RuntimeSystemAction {
                runtime: "orbstack".into(),
                action: "stop".into(),
            });
        }

        #[method(startOrbStackMachine:)]
        fn start_orbstack_machine(&self, sender: &AnyObject) {
            send_orbstack_machine_action(sender, "start");
        }

        #[method(stopOrbStackMachine:)]
        fn stop_orbstack_machine(&self, sender: &AnyObject) {
            send_orbstack_machine_action(sender, "stop");
        }

        #[method(deleteOrbStackMachine:)]
        fn delete_orbstack_machine(&self, sender: &AnyObject) {
            let Some(name) = orbstack_machine_action_name(sender) else {
                return;
            };
            if !confirm_delete_resource("Machine", "OrbStack", &name) {
                return;
            }
            send(CompositorMessage::RuntimeMachineAction {
                runtime: "orbstack".into(),
                name,
                action: "delete".into(),
            });
        }

        #[method(openOrbStackMachineTerminal:)]
        fn open_orbstack_machine_terminal(&self, sender: &AnyObject) {
            let Some(name) = orbstack_machine_action_name(sender) else {
                return;
            };
            send(CompositorMessage::OpenRuntimeMachineTerminal {
                runtime: "orbstack".into(),
                name,
            });
        }

        #[method(useDockerContext:)]
        fn use_docker_context(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let context = DOCKER_CONTEXT_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            let Some(name) = context else {
                show_error_alert(
                    "Docker context action no longer exists. Press Reload and try again.",
                );
                return;
            };
            send(CompositorMessage::UseDockerContext { name });
        }

        #[method(selectContainerImage:)]
        fn select_container_image(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let image = IMAGE_SELECT_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            if let Some(image) = image {
                *SELECTED_NAV.lock().unwrap() = NAV_IMAGES;
                *SELECTED_SESSION.lock().unwrap() = None;
                *SELECTED_IMAGE.lock().unwrap() = Some(image);
                unsafe { rebuild_window(); }
            }
        }

        #[method(selectContainerVolume:)]
        fn select_container_volume(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let volume = VOLUME_SELECT_ACTIONS
                .lock()
                .unwrap()
                .get(tag.max(0) as usize)
                .cloned();
            if let Some(volume) = volume {
                *SELECTED_NAV.lock().unwrap() = NAV_VOLUMES;
                *SELECTED_SESSION.lock().unwrap() = None;
                *SELECTED_VOLUME.lock().unwrap() = Some(volume);
                unsafe { rebuild_window(); }
            }
        }

        #[method(copyContainerCommand:)]
        fn copy_container_command(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let commands = command_items();
            let Some(command) = commands.get(tag.max(0) as usize) else {
                show_error_alert("Command no longer exists.");
                return;
            };
            unsafe {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(&NSString::from_str(command), NSPasteboardTypeString);
            }
            push_activity(format!("Copied command: {}", command));
            unsafe { rebuild_window(); }
        }

        #[method(openAppleContainerDataRoot:)]
        fn open_apple_container_data_root(&self, _sender: &AnyObject) {
            let path = apple_container_data_root();
            let _ = std::process::Command::new("open").arg(path).spawn();
            push_activity("Opened Apple Container data root.".into());
            unsafe { rebuild_window(); }
        }

        #[method(openAppleContainerReleases:)]
        fn open_apple_container_releases(&self, _sender: &AnyObject) {
            match std::process::Command::new("open")
                .arg(APPLE_CONTAINER_RELEASES_URL)
                .spawn()
            {
                Ok(_) => push_activity("Opened the official Apple Container release page.".into()),
                Err(error) => show_error_alert(&format!(
                    "Could not open the Apple Container release page: {}",
                    error
                )),
            }
            unsafe { rebuild_window(); }
        }

        #[method(openOrbStackApp:)]
        fn open_orbstack_app(&self, _sender: &AnyObject) {
            match std::process::Command::new("open")
                .args(["-a", "OrbStack"])
                .spawn()
            {
                Ok(_) => push_activity("Opened OrbStack.".into()),
                Err(error) => show_error_alert(&format!("Could not open OrbStack: {}", error)),
            }
            unsafe { rebuild_window(); }
        }

        #[method(deleteContainerSession:)]
        fn delete_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let index = tag.max(0) as usize;
            unsafe { delete_container_session(index); }
        }

        #[method(selectContainerNav:)]
        fn select_container_nav(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            let selected = tag.max(0) as usize;
            *SELECTED_NAV.lock().unwrap() = selected;
            if !matches!(selected, NAV_SESSIONS | NAV_RUNNING) {
                *SELECTED_SESSION.lock().unwrap() = None;
            }
            if selected != NAV_IMAGES {
                *SELECTED_IMAGE.lock().unwrap() = None;
            }
            if selected != NAV_VOLUMES {
                *SELECTED_VOLUME.lock().unwrap() = None;
            }
            let keep_runtime = SELECTED_RUNTIME_CONTAINER
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|container| runtime_nav(&container.runtime) == selected);
            if !keep_runtime {
                *SELECTED_RUNTIME_CONTAINER.lock().unwrap() = None;
                *RUNTIME_CONTAINER_DETAILS.lock().unwrap() = None;
            }
            unsafe { rebuild_window(); }
        }

        #[method(selectContainerTab:)]
        fn select_container_tab(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            *SELECTED_TAB.lock().unwrap() = tag.max(0) as usize;
            unsafe { rebuild_window(); }
        }

        #[method(selectContainerSession:)]
        fn select_container_session(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            if *SELECTED_NAV.lock().unwrap() != NAV_RUNNING {
                *SELECTED_NAV.lock().unwrap() = NAV_SESSIONS;
            }
            *SELECTED_SESSION.lock().unwrap() = Some(tag.max(0) as usize);
            *SELECTED_IMAGE.lock().unwrap() = None;
            *SELECTED_VOLUME.lock().unwrap() = None;
            *SELECTED_RUNTIME_CONTAINER.lock().unwrap() = None;
            *RUNTIME_CONTAINER_DETAILS.lock().unwrap() = None;
            unsafe { rebuild_window(); }
        }
    }
);

pub fn show(sender: Sender<CompositorMessage>, mtm: MainThreadMarker) {
    *SENDER.lock().unwrap() = Some(sender);

    unsafe {
        ensure_handler();
        let window = ensure_window(mtm);
        install_content(window, mtm);
        window.center();
        window.makeKeyAndOrderFront(None);
    }
}

unsafe fn rebuild_window() {
    let Some(window_ptr) = *WINDOW.lock().unwrap() else {
        return;
    };
    // Container Mode actions are only wired from AppKit controls on the main thread.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };

    let window = unsafe { &*(window_ptr as *mut NSWindow) };
    unsafe {
        install_content(window, mtm);
    }
}

unsafe fn refresh_window_without_focus() {
    let Some(window_ptr) = *WINDOW.lock().unwrap() else {
        return;
    };
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let window = unsafe { &*(window_ptr as *mut NSWindow) };
    unsafe {
        install_content(window, mtm);
    }
}

unsafe fn ensure_handler() -> *mut AnyObject {
    if let Some(ptr) = *HANDLER.lock().unwrap() {
        return ptr as *mut AnyObject;
    }

    let handler: Retained<ContainerModeHandler> =
        unsafe { msg_send_id![ContainerModeHandler::class(), new] };
    let ptr = Retained::into_raw(handler) as *mut AnyObject;
    *HANDLER.lock().unwrap() = Some(ptr as usize);
    ptr
}

unsafe fn ensure_window(mtm: MainThreadMarker) -> &'static NSWindow {
    if let Some(ptr) = *WINDOW.lock().unwrap() {
        return unsafe { &*(ptr as *mut NSWindow) };
    }

    let frame = rect(160.0, 160.0, 1180.0, 760.0);
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSWindow>(),
            frame,
            style,
            NSBackingStoreType::NSBackingStoreBuffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("Cocoa-Way Container Mode"));
    unsafe {
        window.setContentMinSize(NSSize {
            width: 1040.0,
            height: 620.0,
        });
    }
    let handler = unsafe { ensure_handler() };
    let _: () = unsafe { msg_send![&*window, setDelegate: handler] };
    let ptr = Retained::into_raw(window);
    *WINDOW.lock().unwrap() = Some(ptr as usize);
    unsafe { &*ptr }
}

unsafe fn install_content(window: &NSWindow, mtm: MainThreadMarker) {
    unsafe {
        capture_tracked_scroll_position(&LIST_SCROLL_VIEW);
        capture_tracked_scroll_position(&DETAIL_SCROLL_VIEW);
    }
    *SUMMARY_FPS_LABEL.lock().unwrap() = None;
    LIVE_DISPLAY_FPS_LABELS.lock().unwrap().clear();
    let (width, height) = content_size(window);
    let root: Retained<NSView> = unsafe {
        msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, width, height)]
    };
    let load_report = container_sessions::load_sessions_report();
    let sessions = load_report.sessions;
    let config_error = load_report.error;
    schedule_automatic_profile_validation(&sessions);
    let handler = unsafe { ensure_handler() };
    let selected_nav = *SELECTED_NAV.lock().unwrap();
    let selected_tab = *SELECTED_TAB.lock().unwrap();
    let selected_session = selected_session_index(sessions.len(), selected_nav);

    let sidebar_w = 230.0;
    let available_w = (width - sidebar_w).max(760.0);
    let runtime_overview_page = matches!(
        selected_nav,
        NAV_APPLE_CONTAINER | NAV_DOCKER | NAV_ORBSTACK
    );
    let mut list_w = if runtime_overview_page {
        width - sidebar_w
    } else if matches!(selected_nav, NAV_IMAGES | NAV_VOLUMES | NAV_DISPLAYS) {
        (available_w * 0.46).clamp(440.0, 560.0)
    } else {
        (available_w * 0.39).clamp(380.0, 470.0)
    };
    let min_detail_w = 420.0;
    if !runtime_overview_page && width - sidebar_w - list_w < min_detail_w {
        list_w = (width - sidebar_w - min_detail_w).max(360.0);
    }
    let toolbar_h = 64.0;
    let detail_x = sidebar_w + list_w;
    let detail_w = width - detail_x;

    let sidebar = add_sidebar_background(&root, sidebar_w, height, mtm);
    add_resource_sidebar(
        &sidebar,
        sessions.len(),
        height,
        sidebar_w,
        selected_nav,
        handler,
        mtm,
    );
    add_separator(&root, rect(sidebar_w, 0.0, 1.0, height), mtm);

    add_list_toolbar(
        &root,
        sidebar_w,
        height - toolbar_h,
        list_w,
        toolbar_h,
        selected_nav,
        handler,
        mtm,
    );
    add_separator(&root, rect(sidebar_w, height - toolbar_h, list_w, 1.0), mtm);
    if !runtime_overview_page {
        add_separator(&root, rect(detail_x, 0.0, 1.0, height), mtm);
    }

    let overview_summary_height = if runtime_overview_page { 72.0 } else { 0.0 };
    let scroll_frame = rect(
        sidebar_w + 1.0,
        overview_summary_height,
        list_w - 1.0,
        height - toolbar_h - overview_summary_height,
    );
    let scroll = unsafe { NSScrollView::initWithFrame(mtm.alloc::<NSScrollView>(), scroll_frame) };
    unsafe {
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
    }

    let row_height = 180.0;
    let content_w = list_w - 16.0;
    let min_page_height = match selected_nav {
        NAV_APPLE_CONTAINER => 1860.0,
        NAV_ORBSTACK => 1520.0,
        NAV_DOCKER => 1760.0,
        NAV_IMAGES => 1800.0,
        NAV_VOLUMES => 1400.0,
        NAV_DISPLAYS => {
            let managed_rows =
                managed_displays_snapshot().len() + pending_managed_displays_snapshot().len();
            let active_rows = active_sessions_snapshot().len();
            780.0
                + managed_rows as f64 * 150.0
                + active_rows.max(1) as f64 * 116.0
                + sessions.len().max(1).min(16) as f64 * 78.0
        }
        NAV_ACTIVITY => 1560.0,
        NAV_COMMANDS => 1080.0,
        _ => height - toolbar_h,
    };
    let content_height = (sessions.len().max(1) as f64 * row_height + 18.0).max(min_page_height);
    let content: Retained<NSView> = unsafe {
        msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, content_w, content_height)]
    };

    if selected_nav == NAV_IMAGES {
        add_images_list(&content, content_w, content_height, handler, mtm);
    } else if selected_nav == NAV_VOLUMES {
        add_volumes_list(&content, content_w, content_height, handler, mtm);
    } else if selected_nav == NAV_DISPLAYS {
        add_displays_list(&content, content_w, content_height, &sessions, handler, mtm);
    } else if matches!(
        selected_nav,
        NAV_APPLE_CONTAINER | NAV_DOCKER | NAV_ORBSTACK
    ) {
        add_runtime_list(
            &content,
            content_w,
            content_height,
            selected_nav,
            handler,
            mtm,
        );
    } else if selected_nav == NAV_ACTIVITY {
        add_activity_list(&content, content_w, content_height, mtm);
    } else if selected_nav == NAV_COMMANDS {
        add_commands_list(&content, content_w, content_height, handler, mtm);
    } else if !matches!(selected_nav, NAV_SESSIONS | NAV_RUNNING) {
        add_placeholder_list(
            &content,
            content_w,
            content_height,
            nav_title(selected_nav),
            mtm,
        );
    } else if let Some(error) = config_error.as_deref() {
        add_config_error_list(&content, content_w, content_height, error, handler, mtm);
    } else if selected_nav == NAV_RUNNING && active_sessions_snapshot().is_empty() {
        add_running_empty_list(&content, content_w, content_height, mtm);
    } else if sessions.is_empty() {
        add_session_empty_list(&content, content_w, content_height, handler, mtm);
    } else {
        let visible_sessions = sessions
            .iter()
            .enumerate()
            .filter(|(index, _)| selected_nav == NAV_SESSIONS || active_session(*index).is_some())
            .collect::<Vec<_>>();
        for (row, (index, session)) in visible_sessions.into_iter().enumerate() {
            let y = content_height - ((row + 1) as f64 * row_height);
            unsafe {
                add_session_row(
                    &content,
                    session,
                    index,
                    selected_session == Some(index),
                    session_state(index),
                    y,
                    content_w,
                    handler,
                    mtm,
                );
            }
        }
    }

    unsafe {
        scroll.setDocumentView(Some(&content));
        let clip_view: Retained<AnyObject> = msg_send_id![&*scroll, contentView];
        let top_y = (content_height - scroll_frame.size.height).max(0.0);
        let scroll_key = format!("list:{selected_nav}");
        let saved_y = saved_scroll_position(&scroll_key, top_y).clamp(0.0, top_y);
        let _: () = msg_send![&*clip_view, scrollToPoint: NSPoint { x: 0.0, y: saved_y }];
        let _: () = msg_send![&*scroll, reflectScrolledClipView: &*clip_view];
        *LIST_SCROLL_VIEW.lock().unwrap() = Some(TrackedScrollView {
            pointer: (&*scroll as *const NSScrollView) as usize,
            key: scroll_key,
        });
    }
    unsafe {
        root.addSubview(&scroll);
    }
    if runtime_overview_page {
        *DETAIL_SCROLL_VIEW.lock().unwrap() = None;
        add_separator(
            &root,
            rect(sidebar_w, overview_summary_height, list_w, 1.0),
            mtm,
        );
        add_runtime_summary(&root, sidebar_w + 28.0, 4.0, list_w - 56.0, mtm);
    } else {
        add_detail_panel(
            &root,
            detail_x,
            0.0,
            detail_w,
            height,
            selected_tab,
            selected_nav,
            selected_session.and_then(|index| sessions.get(index).map(|session| (index, session))),
            detail_scroll_key(selected_nav, selected_tab, selected_session),
            handler,
            mtm,
        );
    }
    window.setContentView(Some(&root));
}

fn add_sidebar_background(
    parent: &NSView,
    width: f64,
    height: f64,
    mtm: MainThreadMarker,
) -> Retained<NSView> {
    let frame = rect(0.0, 0.0, width, height);
    let content: Retained<NSView> =
        unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: frame] };

    if let Some(glass_class) = AnyClass::get("NSGlassEffectView") {
        let glass: Allocated<NSView> = unsafe { msg_send_id![glass_class, alloc] };
        let glass: Retained<NSView> = unsafe { msg_send_id![glass, initWithFrame: frame] };
        unsafe {
            let _: () = msg_send![&*glass, setStyle: 0isize];
            let _: () = msg_send![&*glass, setCornerRadius: 0.0f64];
            let _: () = msg_send![&*glass, setContentView: &*content];
            parent.addSubview(&glass);
        }
    } else {
        unsafe {
            parent.addSubview(&content);
        }
    }

    content
}

fn selected_session_index(session_count: usize, selected_nav: usize) -> Option<usize> {
    if !matches!(selected_nav, NAV_SESSIONS | NAV_RUNNING) {
        return None;
    }

    let mut selected = SELECTED_SESSION.lock().unwrap();
    match *selected {
        Some(index)
            if index < session_count
                && (selected_nav != NAV_RUNNING || active_session(index).is_some()) =>
        {
            Some(index)
        }
        _ => {
            *selected = None;
            None
        }
    }
}

fn content_size(window: &NSWindow) -> (f64, f64) {
    if let Some(content) = window.contentView() {
        let frame = content.frame();
        return (frame.size.width, frame.size.height);
    }
    (1180.0, 760.0)
}

fn add_resource_sidebar(
    parent: &NSView,
    session_count: usize,
    height: f64,
    width: f64,
    selected_nav: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let compact = height < 700.0;
    let nav_height = if compact { 32.0 } else { 44.0 };
    let session_y = if compact {
        height - 132.0
    } else {
        height - 144.0
    };
    let nav_step = if compact { 34.0 } else { 46.0 };
    let resources_heading_y = if compact {
        height - 194.0
    } else {
        height - 230.0
    };
    let resources_first_y = if compact {
        height - 228.0
    } else {
        height - 272.0
    };
    let runtime_heading_y = if compact {
        height - 324.0
    } else {
        height - 410.0
    };
    let runtime_first_y = if compact {
        height - 358.0
    } else {
        height - 454.0
    };
    let general_heading_y = if compact {
        height - 426.0
    } else {
        height - 548.0
    };
    let general_first_y = if compact {
        height - 460.0
    } else {
        height - 592.0
    };
    add_label(
        parent,
        "Cocoa-Way",
        rect(18.0, height - 52.0, width - 36.0, 24.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Applications",
        rect(18.0, height - 96.0, width - 36.0, 18.0),
        mtm,
        TextStyle::Section,
    );
    add_nav_item(
        parent,
        NAV_SESSIONS,
        "Applications",
        &format!("{} profiles", session_count),
        selected_nav == NAV_SESSIONS,
        rect(10.0, session_y, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_label(
        parent,
        "Resources",
        rect(18.0, resources_heading_y, width - 36.0, 18.0),
        mtm,
        TextStyle::Section,
    );
    add_nav_item(
        parent,
        NAV_RUNNING,
        "Running",
        &format!("{} active", active_sessions_snapshot().len()),
        selected_nav == NAV_RUNNING,
        rect(10.0, session_y - nav_step, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_nav_item(
        parent,
        NAV_IMAGES,
        "Images",
        "GUI-ready images",
        selected_nav == NAV_IMAGES,
        rect(10.0, resources_first_y, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_nav_item(
        parent,
        NAV_VOLUMES,
        "Volumes",
        "shared data",
        selected_nav == NAV_VOLUMES,
        rect(10.0, resources_first_y - nav_step, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_nav_item(
        parent,
        NAV_DISPLAYS,
        "Displays",
        "window slots",
        selected_nav == NAV_DISPLAYS,
        rect(
            10.0,
            resources_first_y - nav_step * 2.0,
            width - 20.0,
            nav_height,
        ),
        handler,
        mtm,
    );

    add_label(
        parent,
        "Runtimes",
        rect(18.0, runtime_heading_y, width - 36.0, 18.0),
        mtm,
        TextStyle::Section,
    );
    add_nav_item(
        parent,
        NAV_APPLE_CONTAINER,
        "Apple Container",
        "first-class target",
        selected_nav == NAV_APPLE_CONTAINER,
        rect(10.0, runtime_first_y, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_nav_item(
        parent,
        NAV_DOCKER,
        "Docker-compatible",
        "contexts and providers",
        selected_nav == NAV_DOCKER,
        rect(10.0, runtime_first_y - nav_step, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_label(
        parent,
        "Diagnostics",
        rect(18.0, general_heading_y, width - 36.0, 18.0),
        mtm,
        TextStyle::Section,
    );
    let activity_count = activity_snapshot().len();
    let running_tasks = active_task_count();
    let activity_subtitle = if running_tasks > 0 {
        format!("{} running · {} events", running_tasks, activity_count)
    } else {
        format!("{} events", activity_count)
    };
    add_nav_item(
        parent,
        NAV_ACTIVITY,
        "Activity",
        &activity_subtitle,
        selected_nav == NAV_ACTIVITY,
        rect(10.0, general_first_y, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_nav_item(
        parent,
        NAV_COMMANDS,
        "Commands",
        "launch helpers",
        selected_nav == NAV_COMMANDS,
        rect(10.0, general_first_y - nav_step, width - 20.0, nav_height),
        handler,
        mtm,
    );
    add_separator(parent, rect(0.0, 70.0, width, 1.0), mtm);
    let settings = add_button(
        parent,
        "Settings...",
        rect(18.0, 24.0, 104.0, 28.0),
        handler,
        sel!(openContainerSettings:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*settings, setToolTip:
            &*NSString::from_str("Open Cocoa-Way Container Mode settings")];
    }
}

fn add_nav_item(
    parent: &NSView,
    index: usize,
    title: &str,
    subtitle: &str,
    active: bool,
    frame: NSRect,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    if active {
        add_card(parent, frame, mtm);
        add_runtime_accent(
            parent,
            index,
            rect(
                frame.origin.x,
                frame.origin.y + 8.0,
                3.0,
                frame.size.height - 16.0,
            ),
            mtm,
        );
    }
    if frame.size.height < 40.0 {
        add_label(
            parent,
            title,
            rect(
                frame.origin.x + 16.0,
                frame.origin.y + 7.0,
                frame.size.width - 32.0,
                20.0,
            ),
            mtm,
            TextStyle::Heading,
        );
    } else {
        add_label(
            parent,
            title,
            rect(
                frame.origin.x + 16.0,
                frame.origin.y + 22.0,
                frame.size.width - 32.0,
                20.0,
            ),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            subtitle,
            rect(
                frame.origin.x + 16.0,
                frame.origin.y + 7.0,
                frame.size.width - 32.0,
                16.0,
            ),
            mtm,
            TextStyle::Caption,
        );
    }
    add_hit_button(
        parent,
        frame,
        index,
        handler,
        sel!(selectContainerNav:),
        mtm,
    );
}

fn add_list_toolbar(
    parent: &NSView,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    selected_nav: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let title_width = if selected_nav == NAV_SESSIONS {
        (width - 238.0).max(126.0)
    } else if selected_nav == NAV_ACTIVITY {
        (width - 190.0).max(110.0)
    } else {
        (width - 116.0).max(110.0)
    };
    add_label(
        parent,
        nav_title(selected_nav),
        rect(x + 18.0, y + 18.0, title_width, 30.0),
        mtm,
        TextStyle::Title,
    );
    let reload = add_button(
        parent,
        "Reload",
        rect(x + width - 86.0, y + 17.0, 68.0, 30.0),
        handler,
        sel!(reloadContainerMode:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*reload, setToolTip:
            &*NSString::from_str("Reload container-sessions.toml")];
    }
    if selected_nav == NAV_ACTIVITY {
        let clear = add_button(
            parent,
            "Clear",
            rect(x + width - 168.0, y + 17.0, 74.0, 30.0),
            handler,
            sel!(clearContainerActivity:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*clear, setToolTip:
                &*NSString::from_str("Clear Container Mode activity messages")];
        }
    }
    if selected_nav == NAV_SESSIONS {
        let open = add_button(
            parent,
            "New Application",
            rect(x + width - 212.0, y + 17.0, 116.0, 30.0),
            handler,
            sel!(addContainerSession:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*open, setToolTip:
                &*NSString::from_str("Create a saved application profile")];
        }
    }
    let _ = height;
}

fn add_session_empty_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let center_y = (content_height * 0.52).max(250.0);
    add_label(
        parent,
        "No Applications",
        rect(34.0, center_y, width - 68.0, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        "Restore the bundled Niri desktop profile or create a custom Wayland application profile.",
        rect(34.0, center_y - 42.0, width - 68.0, 42.0),
        mtm,
        TextStyle::Body,
    );
    add_label(
        parent,
        "Example\nruntime = \"container\"\nimage = \"localhost/cocoa-way-niri:latest\"\nprofile = \"niri\"\ncommand = \"niri\"",
        rect(34.0, center_y - 142.0, width - 68.0, 90.0),
        mtm,
        TextStyle::Mono,
    );
    let restore = add_button(
        parent,
        "Restore Example",
        rect(34.0, center_y - 190.0, 128.0, 30.0),
        handler,
        sel!(restoreSmokeContainerSession:),
        mtm,
    );
    let add = add_button(
        parent,
        "Custom...",
        rect(174.0, center_y - 190.0, 96.0, 30.0),
        handler,
        sel!(addContainerSession:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*restore, setToolTip:
            &*NSString::from_str("Restore the bundled example application profile")];
        let _: () = msg_send![&*add, setToolTip:
            &*NSString::from_str("Create a custom Container Mode application")];
    }
}

fn add_running_empty_list(parent: &NSView, width: f64, content_height: f64, mtm: MainThreadMarker) {
    let center_y = (content_height * 0.52).max(250.0);
    add_label(
        parent,
        "No Running Applications",
        rect(34.0, center_y, width - 68.0, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        "Launch an application profile to create an instance. Active containers, Waypipe workers, and display attachments will appear here.",
        rect(34.0, center_y - 62.0, width - 68.0, 54.0),
        mtm,
        TextStyle::Body,
    );
}

fn add_config_error_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    error: &str,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let center_y = (content_height * 0.52).max(250.0);
    add_label(
        parent,
        "Config Error",
        rect(34.0, center_y, width - 68.0, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        "Container Mode could not parse container-sessions.toml. Fix the file and press Reload.",
        rect(34.0, center_y - 42.0, width - 68.0, 42.0),
        mtm,
        TextStyle::Body,
    );
    add_label(
        parent,
        error,
        rect(34.0, center_y - 128.0, width - 68.0, 72.0),
        mtm,
        TextStyle::Mono,
    );
    let open = add_button(
        parent,
        "Open Config",
        rect(34.0, center_y - 176.0, 116.0, 30.0),
        handler,
        sel!(openContainerConfig:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*open, setToolTip:
            &*NSString::from_str("Reveal container-sessions.toml in Finder")];
    }
}

fn add_activity_list(parent: &NSView, width: f64, content_height: f64, mtm: MainThreadMarker) {
    let activity = activity_snapshot();
    let tasks = operation_tasks_snapshot();
    let mut y = content_height - 58.0;
    add_label(
        parent,
        "Performance",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 44.0;
    add_card(parent, rect(24.0, y - 174.0, width - 48.0, 204.0), mtm);
    if let Some(snapshot) = performance_snapshot() {
        add_label(
            parent,
            &format!(
                "Render {:.1} fps  |  commits {:.1}/s  |  late {:.1}/s",
                snapshot.redraw_fps, snapshot.commits_per_second, snapshot.late_redraws_per_second
            ),
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Body,
        );
        add_label(
            parent,
            &format!(
                "Max redraw wait {:.1} ms  |  host input -> present {}",
                snapshot.max_redraw_wait_ms,
                snapshot
                    .input_to_present_ms
                    .map(|value| format!("{value:.1} ms"))
                    .unwrap_or_else(|| "waiting".into()),
            ),
            rect(38.0, y - 26.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        add_label(
            parent,
            &format!(
                "Scene {} tile(s)  |  dirty {}  |  callbacks {}",
                snapshot.tiles,
                if snapshot.dirty { "yes" } else { "no" },
                snapshot.pending_frame_callbacks
            ),
            rect(38.0, y - 50.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
    } else {
        add_label(
            parent,
            "No performance sample yet. Launch an application or wait for the next render tick.",
            rect(38.0, y - 8.0, width - 76.0, 36.0),
            mtm,
            TextStyle::Caption,
        );
    }
    let resources = crate::diagnostics::resource_snapshot();
    let resource_line = if resources.available {
        format!(
            "Apple containers {}  |  CPU {}  |  memory {:.2} / {:.2} GiB",
            resources.containers.len(),
            resources
                .total_cpu_percent
                .map(|value| format!("{value:.1}%"))
                .unwrap_or_else(|| "sampling".into()),
            crate::diagnostics::bytes_to_gib(resources.total_memory_usage_bytes),
            crate::diagnostics::bytes_to_gib(resources.total_memory_limit_bytes),
        )
    } else {
        format!(
            "Apple container resources: {}",
            resources.error.as_deref().unwrap_or("unavailable")
        )
    };
    add_label(
        parent,
        &resource_line,
        rect(38.0, y - 78.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let clipboard = crate::diagnostics::clipboard_snapshot();
    add_label(
        parent,
        &format!(
            "Clipboard {}  |  H->G {}  |  G->H {}  |  errors {}",
            clipboard.last_direction.as_deref().unwrap_or("waiting"),
            clipboard.host_to_guest_events,
            clipboard.guest_to_host_events,
            clipboard.failures,
        ),
        rect(38.0, y - 102.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        &format!(
            "Apple Container storage: {}",
            resources
                .disk_available_bytes
                .map(|bytes| format!("{:.1} GiB free", crate::diagnostics::bytes_to_gib(bytes)))
                .unwrap_or_else(|| "unknown".into())
        ),
        rect(38.0, y - 126.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    y -= 236.0;

    if !tasks.is_empty() {
        add_label(
            parent,
            "Operations",
            rect(34.0, y, width - 68.0, 24.0),
            mtm,
            TextStyle::Title,
        );
        y -= 40.0;
        for task in tasks.iter().rev().take(4) {
            let completed = task
                .steps
                .iter()
                .filter(|step| step.status == TaskStepStatus::Completed)
                .count();
            let current = task
                .steps
                .iter()
                .find(|step| {
                    matches!(
                        step.status,
                        TaskStepStatus::Running | TaskStepStatus::Failed
                    )
                })
                .map(|step| step.name.as_str())
                .unwrap_or_else(|| {
                    if task.status == TaskStatus::Completed {
                        "Complete"
                    } else {
                        "Queued"
                    }
                });
            add_card(parent, rect(24.0, y - 66.0, width - 48.0, 84.0), mtm);
            add_label(
                parent,
                &format!("{} · {}", task.operation, task.subject),
                rect(38.0, y - 2.0, width - 76.0, 20.0),
                mtm,
                TextStyle::Heading,
            );
            add_label(
                parent,
                &format!(
                    "{} · {}/{} steps · {}",
                    task.status.label(),
                    completed,
                    task.steps.len(),
                    current
                ),
                rect(38.0, y - 28.0, width - 76.0, 18.0),
                mtm,
                TextStyle::Caption,
            );
            if let Some(detail) = task.detail.as_deref() {
                add_label(
                    parent,
                    detail,
                    rect(38.0, y - 52.0, width - 76.0, 18.0),
                    mtm,
                    TextStyle::Caption,
                );
            }
            y -= 98.0;
        }
        y -= 16.0;
    }

    if activity.is_empty() {
        add_label(
            parent,
            "No recent activity",
            rect(34.0, y, width - 68.0, 24.0),
            mtm,
            TextStyle::Title,
        );
        return;
    }

    add_label(
        parent,
        "Recent Activity",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 42.0;
    for line in activity.iter().rev().take(12) {
        add_card(parent, rect(24.0, y - 10.0, width - 48.0, 42.0), mtm);
        add_label(
            parent,
            line,
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 52.0;
    }
}

fn add_images_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let inventories = image_inventories();
    let registry_summary = apple_registry_login_summary(&build_child_path());
    IMAGE_CREATE_ACTIONS.lock().unwrap().clear();
    IMAGE_DELETE_ACTIONS.lock().unwrap().clear();
    IMAGE_SELECT_ACTIONS.lock().unwrap().clear();
    let selected_image = SELECTED_IMAGE.lock().unwrap().clone();
    let mut y = content_height - 58.0;
    add_label(
        parent,
        "Local Images",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 42.0;

    add_card(parent, rect(24.0, y - 140.0, width - 48.0, 170.0), mtm);
    add_label(
        parent,
        "Sources & images",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Pull from Docker Hub, GHCR, Quay, or any OCI registry.",
        rect(38.0, y - 20.0, width - 76.0, 32.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        &registry_summary,
        rect(38.0, y - 46.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let button_area = width - 76.0;
    let half = ((button_area - 10.0) / 2.0).max(80.0);
    let pull = add_button(
        parent,
        "Pull from Source...",
        rect(38.0, y - 78.0, half, 28.0),
        handler,
        sel!(pullContainerImage:),
        mtm,
    );
    let login = add_button(
        parent,
        "Registry Login...",
        rect(48.0 + half, y - 78.0, half, 28.0),
        handler,
        sel!(loginContainerRegistry:),
        mtm,
    );
    let build = add_button(
        parent,
        "Build Example",
        rect(38.0, y - 114.0, half, 28.0),
        handler,
        sel!(buildSmokeContainerImage:),
        mtm,
    );
    let load = add_button(
        parent,
        "Import OCI Archive...",
        rect(48.0 + half, y - 114.0, half, 28.0),
        handler,
        sel!(loadContainerImage:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*build, setToolTip:
            &*NSString::from_str("Build the bundled example image with Apple Container")];
        let _: () = msg_send![&*pull, setToolTip:
            &*NSString::from_str("Choose a registry, platform, destination, and post-pull action")];
        let _: () = msg_send![&*login, setToolTip:
            &*NSString::from_str("Log in to a private OCI registry through Apple Container")];
        let _: () = msg_send![&*load, setToolTip:
            &*NSString::from_str("Load an OCI image tar archive into Apple Container")];
    }
    y -= 202.0;

    for inventory in inventories {
        add_label(
            parent,
            inventory.runtime,
            rect(34.0, y, width - 68.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        y -= 38.0;

        for row in inventory.rows.iter().take(10) {
            if row.reference.is_none() {
                add_label(
                    parent,
                    &row.label,
                    rect(38.0, y + 4.0, width - 76.0, 18.0),
                    mtm,
                    TextStyle::Caption,
                );
                y -= 40.0;
                continue;
            }
            let selected = selected_image.as_ref().is_some_and(|selected| {
                selected.reference == row.reference.clone().unwrap_or_default()
            });
            add_card(parent, rect(24.0, y - 42.0, width - 48.0, 76.0), mtm);
            if selected {
                add_separator(parent, rect(24.0, y - 42.0, 4.0, 76.0), mtm);
            }
            add_label(
                parent,
                &row.label,
                rect(38.0, y + 12.0, width - 76.0, 18.0),
                mtm,
                if row.reference.is_some() {
                    TextStyle::Mono
                } else {
                    TextStyle::Caption
                },
            );
            if let Some(reference) = row.reference.as_ref() {
                let select_index = {
                    let mut actions = IMAGE_SELECT_ACTIONS.lock().unwrap();
                    let action_index = actions.len();
                    actions.push(SelectedImage {
                        runtime: inventory.runtime.to_string(),
                        runtime_key: inventory.runtime_key.to_string(),
                        reference: reference.clone(),
                        label: row.label.clone(),
                    });
                    action_index
                };
                add_hit_button(
                    parent,
                    rect(24.0, y - 42.0, width - 48.0, 76.0),
                    select_index,
                    handler,
                    sel!(selectContainerImage:),
                    mtm,
                );
                let create_index = {
                    let mut actions = IMAGE_CREATE_ACTIONS.lock().unwrap();
                    let action_index = actions.len();
                    actions.push((inventory.runtime_key.to_string(), reference.clone()));
                    action_index
                };
                let create = add_button(
                    parent,
                    "Create Application",
                    rect(38.0, y - 24.0, 142.0, 28.0),
                    handler,
                    sel!(createContainerSessionFromImage:),
                    mtm,
                );
                unsafe {
                    let _: () = msg_send![&*create, setTag: create_index as isize];
                    let _: () = msg_send![&*create, setToolTip:
                        &*NSString::from_str("Create an application profile from this image")];
                }
                let delete_index = {
                    let mut actions = IMAGE_DELETE_ACTIONS.lock().unwrap();
                    let action_index = actions.len();
                    actions.push(ImageDeleteAction {
                        runtime: inventory.runtime_key.to_string(),
                        reference: reference.clone(),
                        image_id: image_id_from_label(&row.label, reference),
                    });
                    action_index
                };
                let image_has_tag = image_reference_has_tag(reference);
                let more = add_popup(
                    parent,
                    rect(190.0, y - 24.0, 112.0, 28.0),
                    if image_has_tag {
                        &["More…", "Remove Tag", "Delete Image"]
                    } else {
                        &["More…", "Delete Image"]
                    },
                    0,
                    mtm,
                );
                unsafe {
                    let _: () = msg_send![&*more, setTarget: handler];
                    let _: () = msg_send![&*more, setAction: sel!(imageMoreAction:)];
                    let _: () = msg_send![&*more, setTag: delete_index as isize];
                    let _: () = msg_send![&*more, setToolTip:
                        &*NSString::from_str("Remove a tag or delete the underlying image after dependency checks")];
                }
            }
            y -= 86.0;
        }
        y -= 18.0;
    }

    add_label(
        parent,
        "Maintenance",
        rect(34.0, y, width - 68.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    y -= 38.0;
    add_card(parent, rect(24.0, y - 42.0, width - 48.0, 76.0), mtm);
    add_label(
        parent,
        "Delete a local image",
        rect(38.0, y + 12.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Body,
    );
    add_label(
        parent,
        "Destructive image cleanup is separate from creating an application profile.",
        rect(38.0, y - 10.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let delete = add_button(
        parent,
        "Delete Image...",
        rect(38.0, y - 42.0, 120.0, 28.0),
        handler,
        sel!(deleteContainerImage:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*delete, setToolTip:
            &*NSString::from_str("Delete a local image by runtime and reference")];
    }
}

fn add_volumes_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let inventories = volume_inventories();
    VOLUME_DELETE_ACTIONS.lock().unwrap().clear();
    VOLUME_SELECT_ACTIONS.lock().unwrap().clear();
    let selected_volume = SELECTED_VOLUME.lock().unwrap().clone();
    let mut y = content_height - 58.0;
    add_label(
        parent,
        "Local Volumes",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 42.0;

    add_card(parent, rect(24.0, y - 58.0, width - 48.0, 92.0), mtm);
    add_label(
        parent,
        "Volume actions",
        rect(38.0, y + 10.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Create persistent storage in Apple Container or the active Docker context.",
        rect(38.0, y - 16.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let create = add_button(
        parent,
        "Create Volume...",
        rect(38.0, y - 52.0, 126.0, 28.0),
        handler,
        sel!(createContainerVolume:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*create, setToolTip:
            &*NSString::from_str("Create a named volume in Apple Container or Docker")];
    }
    y -= 126.0;

    for inventory in inventories {
        add_label(
            parent,
            inventory.runtime,
            rect(34.0, y, width - 68.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        y -= 38.0;

        for row in inventory.rows.iter().take(10) {
            if row.name.is_none() {
                if row.label.starts_with("No ") {
                    add_card(parent, rect(24.0, y - 84.0, width - 48.0, 116.0), mtm);
                    add_label(
                        parent,
                        &row.label,
                        rect(38.0, y + 8.0, width - 76.0, 20.0),
                        mtm,
                        TextStyle::Heading,
                    );
                    add_label(
                        parent,
                        "Volumes preserve application data between launches.",
                        rect(38.0, y - 20.0, width - 76.0, 20.0),
                        mtm,
                        TextStyle::Caption,
                    );
                    add_button(
                        parent,
                        "Create Volume",
                        rect(38.0, y - 62.0, 116.0, 28.0),
                        handler,
                        sel!(createContainerVolume:),
                        mtm,
                    );
                    y -= 136.0;
                } else {
                    add_label(
                        parent,
                        &row.label,
                        rect(38.0, y + 4.0, width - 76.0, 36.0),
                        mtm,
                        TextStyle::Caption,
                    );
                    y -= 52.0;
                }
                continue;
            }
            let selected = selected_volume.as_ref().is_some_and(|selected| {
                selected.name == row.name.clone().unwrap_or_default()
                    && selected.runtime_key == inventory.runtime_key
            });
            add_card(parent, rect(24.0, y - 62.0, width - 48.0, 96.0), mtm);
            if selected {
                add_separator(parent, rect(24.0, y - 62.0, 4.0, 96.0), mtm);
            }
            let name = row.name.as_ref().unwrap();
            let usage = volume_usage(inventory.runtime_key, name);
            let metadata = volume_inspect_metadata(inventory.runtime_key, name, &row.label);
            let usage_summary = if usage.loading {
                "Checking profile and container usage...".into()
            } else if let Some(error) = usage.error.as_deref() {
                short_text(error, 56)
            } else {
                format!(
                    "{} referenced · {} mounted",
                    usage.referenced_profiles.len(),
                    usage.mounted_containers.len()
                )
            };
            add_label(
                parent,
                name,
                rect(38.0, y + 12.0, width - 154.0, 20.0),
                mtm,
                TextStyle::Heading,
            );
            add_label(
                parent,
                &format!(
                    "{} · {} · {}",
                    metadata.kind, metadata.size, metadata.created
                ),
                rect(38.0, y - 10.0, width - 154.0, 18.0),
                mtm,
                TextStyle::Caption,
            );
            add_label(
                parent,
                &usage_summary,
                rect(38.0, y - 34.0, width - 154.0, 18.0),
                mtm,
                TextStyle::Caption,
            );
            if let Some(name) = row.name.as_ref() {
                let select_index = {
                    let mut actions = VOLUME_SELECT_ACTIONS.lock().unwrap();
                    let action_index = actions.len();
                    actions.push(SelectedVolume {
                        runtime: inventory.runtime.to_string(),
                        runtime_key: inventory.runtime_key.to_string(),
                        name: name.clone(),
                        label: row.label.clone(),
                    });
                    action_index
                };
                add_hit_button(
                    parent,
                    rect(24.0, y - 62.0, width - 48.0, 96.0),
                    select_index,
                    handler,
                    sel!(selectContainerVolume:),
                    mtm,
                );
                let action_index = {
                    let mut actions = VOLUME_DELETE_ACTIONS.lock().unwrap();
                    let action_index = actions.len();
                    actions.push(VolumeDeleteAction {
                        runtime: inventory.runtime_key.to_string(),
                        name: name.clone(),
                    });
                    action_index
                };
                let delete = add_button(
                    parent,
                    "Delete",
                    rect(width - 116.0, y - 38.0, 78.0, 28.0),
                    handler,
                    sel!(deleteLocalContainerVolume:),
                    mtm,
                );
                unsafe {
                    let _: () = msg_send![&*delete, setTag: action_index as isize];
                    let blocked = usage.loading
                        || usage.error.is_some()
                        || !usage.mounted_containers.is_empty();
                    let _: () = msg_send![&*delete, setEnabled: !blocked];
                    let tooltip = if !usage.mounted_containers.is_empty() {
                        format!("Mounted by: {}", usage.mounted_containers.join(", "))
                    } else if usage.loading {
                        "Wait for the volume usage check to finish".into()
                    } else if let Some(error) = usage.error.as_deref() {
                        format!("Usage could not be verified: {error}")
                    } else {
                        "Delete this local volume after a dependency check".into()
                    };
                    let _: () = msg_send![&*delete, setToolTip: &*NSString::from_str(&tooltip)];
                }
            }
            y -= 106.0;
        }
        y -= 18.0;
    }
}

fn add_displays_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    sessions: &[ContainerSession],
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    MANAGED_DISPLAY_ACTIONS.lock().unwrap().clear();
    let managed_displays = managed_displays_snapshot();
    let pending_displays = pending_managed_displays_snapshot();
    let closing_displays = closing_managed_displays_snapshot();
    let active_sessions = active_sessions_snapshot();
    let mut y = content_height - 58.0;
    add_label(
        parent,
        "Managed Displays",
        rect(34.0, y, width - 210.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    add_button(
        parent,
        "Create Display",
        rect(width - 154.0, y - 4.0, 120.0, 30.0),
        handler,
        sel!(createManagedDisplay:),
        mtm,
    );
    y -= 36.0;
    add_label(
        parent,
        "Persistent Cocoa-Way windows for explicit local, remote, and application connections.",
        rect(34.0, y - 30.0, width - 68.0, 44.0),
        mtm,
        TextStyle::Caption,
    );
    y -= 58.0;

    for slot in &pending_displays {
        add_card(parent, rect(24.0, y - 42.0, width - 48.0, 70.0), mtm);
        add_label(
            parent,
            slot,
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            &format!(
                "{} · Starting an independent Wayland display window...",
                DisplayStatus::Allocating.label()
            ),
            rect(38.0, y - 24.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 80.0;
    }

    if managed_displays.is_empty() && pending_displays.is_empty() {
        add_card(parent, rect(24.0, y - 58.0, width - 48.0, 86.0), mtm);
        add_label(
            parent,
            "No managed displays",
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            "The default display still works. Create one when external and GUI-managed connections need explicit allocation.",
            rect(38.0, y - 40.0, width - 76.0, 38.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 98.0;
    }

    for display in &managed_displays {
        let attachment_count = active_sessions
            .iter()
            .filter(|active| active.instance.display_slot == display.slot)
            .count();
        let closing = closing_displays.iter().any(|slot| slot == &display.slot);
        let status = if closing {
            DisplayStatus::Closing
        } else if attachment_count > 0 {
            DisplayStatus::Attached
        } else {
            DisplayStatus::Free
        };
        let action_index = {
            let mut actions = MANAGED_DISPLAY_ACTIONS.lock().unwrap();
            let index = actions.len();
            actions.push(display.clone());
            index
        };
        add_card(parent, rect(24.0, y - 112.0, width - 48.0, 140.0), mtm);
        add_label(
            parent,
            &display.slot,
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        let display_performance = worker_performance_snapshot(&display.runtime_dir);
        let performance_base = format!(
            "{} · {} attachment{} · process {}",
            status.label(),
            attachment_count,
            if attachment_count == 1 { "" } else { "s" },
            display.pid,
        );
        let performance_label = add_label(
            parent,
            &display_fps_text(&performance_base, display_performance.as_ref()),
            rect(38.0, y - 22.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        register_live_display_fps_label(&performance_label, &display.slot, performance_base);
        add_label(
            parent,
            &short_text(
                &format!("{}/{}", display.runtime_dir, display.display),
                chars_for_width(width - 76.0, TextStyle::Mono),
            ),
            rect(38.0, y - 46.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        let focus = add_button(
            parent,
            "Focus Window",
            rect(38.0, y - 104.0, 100.0, 28.0),
            handler,
            sel!(focusManagedDisplay:),
            mtm,
        );
        let copy_command = add_button(
            parent,
            "Copy Connection",
            rect(146.0, y - 104.0, 116.0, 28.0),
            handler,
            sel!(copyManagedDisplayCommand:),
            mtm,
        );
        let close = add_button(
            parent,
            "Close Display",
            rect(width - 146.0, y - 104.0, 112.0, 28.0),
            handler,
            sel!(closeManagedDisplay:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*focus, setTag: action_index as isize];
            let _: () = msg_send![&*copy_command, setTag: action_index as isize];
            let _: () = msg_send![&*close, setTag: action_index as isize];
            let _: () = msg_send![&*focus, setEnabled: !closing];
            let _: () = msg_send![&*copy_command, setEnabled: !closing];
            let _: () = msg_send![&*close, setEnabled: !closing && attachment_count == 0];
            let _: () = msg_send![&*focus, setToolTip:
                &*NSString::from_str("Bring this independent display window to the front")];
            let _: () = msg_send![&*copy_command, setToolTip:
                &*NSString::from_str("Copy a run_waypipe.sh command prefix for this display")];
            let close_tooltip = if attachment_count > 0 {
                "Release the active attachment before closing this display"
            } else {
                "Close this Cocoa-Way display window"
            };
            let _: () = msg_send![&*close, setToolTip: &*NSString::from_str(close_tooltip)];
        }
        y -= 150.0;
    }

    if let Some(error) = MANAGED_DISPLAY_LAST_ERROR.lock().unwrap().as_deref() {
        add_card(parent, rect(24.0, y - 54.0, width - 48.0, 82.0), mtm);
        add_label(
            parent,
            &format!("Last display operation · {}", DisplayStatus::Failed.label()),
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            error,
            rect(38.0, y - 46.0, width - 76.0, 42.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 94.0;
    }

    y -= 18.0;
    add_label(
        parent,
        "Active Attachments",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 30.0;
    add_label(
        parent,
        "Runtime bindings between an application instance and a Cocoa-Way display.",
        rect(34.0, y - 22.0, width - 68.0, 32.0),
        mtm,
        TextStyle::Caption,
    );
    y -= 42.0;
    if active_sessions.is_empty() {
        add_card(parent, rect(24.0, y - 42.0, width - 48.0, 70.0), mtm);
        add_label(
            parent,
            "No active display attachments",
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            "Launch an application to allocate a display at runtime.",
            rect(38.0, y - 24.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 82.0;
    } else {
        for active in &active_sessions {
            let session = sessions.get(active.instance.profile_index);
            let session_name = session
                .map(|session| session.name.as_str())
                .unwrap_or("Unknown application");
            let presentation = session
                .map(session_presentation_summary)
                .unwrap_or("unknown");
            add_card(parent, rect(24.0, y - 76.0, width - 48.0, 104.0), mtm);
            add_label(
                parent,
                session_name,
                rect(38.0, y + 2.0, width - 190.0, 20.0),
                mtm,
                TextStyle::Heading,
            );
            add_label(
                parent,
                &format!(
                    "{} · instance #{} · {} · {}",
                    DisplayStatus::Attached.label(),
                    active.instance.id,
                    active.instance.display_slot,
                    presentation
                ),
                rect(38.0, y - 24.0, width - 190.0, 18.0),
                mtm,
                TextStyle::Caption,
            );
            let display_performance = performance_for_active_session(active);
            let performance_base = format!(
                "waypipe {} · display process {}",
                active.instance.waypipe_pid,
                active
                    .instance
                    .display_pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "built-in".into()),
            );
            let performance_label = add_label(
                parent,
                &display_fps_text(&performance_base, display_performance.as_ref()),
                rect(38.0, y - 50.0, width - 190.0, 18.0),
                mtm,
                TextStyle::Mono,
            );
            register_live_display_fps_label(
                &performance_label,
                &active.instance.display_slot,
                performance_base,
            );
            let release = add_button(
                parent,
                "Release Display",
                rect(width - 150.0, y - 28.0, 116.0, 28.0),
                handler,
                sel!(releaseDisplayAttachment:),
                mtm,
            );
            unsafe {
                let _: () = msg_send![&*release, setTag: active.instance.profile_index as isize];
                let _: () = msg_send![&*release, setToolTip:
                    &*NSString::from_str("Stop this application instance and release its display")];
            }
            y -= 116.0;
        }
    }

    y -= 18.0;
    add_label(
        parent,
        "Built-in Display",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 42.0;
    let default_attachments = active_sessions
        .iter()
        .filter(|active| active.instance.display_slot == "default")
        .count();
    let built_in_status = if default_attachments > 0 {
        DisplayStatus::Attached
    } else {
        DisplayStatus::Free
    };
    add_card(parent, rect(24.0, y - 86.0, width - 48.0, 114.0), mtm);
    add_label(
        parent,
        "Default Display",
        rect(38.0, y + 2.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        &format!(
            "{} · {} active attachment{}",
            built_in_status.label(),
            default_attachments,
            if default_attachments == 1 { "" } else { "s" }
        ),
        rect(38.0, y - 24.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        "The current Cocoa-Way Metal window. Auto uses it while free, then allocates an isolated display.",
        rect(38.0, y - 76.0, width - 76.0, 44.0),
        mtm,
        TextStyle::Caption,
    );
    y -= 132.0;

    add_label(
        parent,
        "Display Policies",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 30.0;
    add_label(
        parent,
        "Saved profile preferences. Policies do not reserve a display until an instance launches.",
        rect(34.0, y - 22.0, width - 68.0, 32.0),
        mtm,
        TextStyle::Caption,
    );
    y -= 42.0;
    if sessions.is_empty() {
        add_card(parent, rect(24.0, y - 42.0, width - 48.0, 70.0), mtm);
        add_label(
            parent,
            "No display policies",
            rect(38.0, y + 2.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            "Create an application profile to configure its display policy.",
            rect(38.0, y - 24.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        return;
    }
    for session in sessions.iter().take(16) {
        let target = resolved_session_display_target(session);
        let policy = if target == "automatic" {
            "Auto allocation".to_string()
        } else if target == "default" {
            "Pinned to built-in display".to_string()
        } else {
            format!("Named display: {target}")
        };
        add_card(parent, rect(24.0, y - 36.0, width - 48.0, 68.0), mtm);
        add_label(
            parent,
            &session.name,
            rect(38.0, y + 8.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            &format!(
                "{} · {} presentation",
                policy,
                session_presentation_summary(session)
            ),
            rect(38.0, y - 16.0, width - 76.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 78.0;
    }
}

fn add_runtime_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    selected_nav: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    RUNTIME_CONTAINER_ACTIONS.lock().unwrap().clear();
    RUNTIME_CONTAINER_SELECT_ACTIONS.lock().unwrap().clear();
    DOCKER_CONTEXT_ACTIONS.lock().unwrap().clear();
    ORBSTACK_MACHINE_ACTIONS.lock().unwrap().clear();
    let runtime = match selected_nav {
        NAV_APPLE_CONTAINER => RuntimeInfoTarget {
            title: "Apple Container",
            command: "container",
            checks: vec![
                RuntimeCheck::new("Version", &["--version"]),
                RuntimeCheck::new("System", &["system", "status"]),
                RuntimeCheck::new("Images", &["image", "list"]),
            ],
        },
        NAV_DOCKER => RuntimeInfoTarget {
            title: "Docker",
            command: "docker",
            checks: vec![
                RuntimeCheck::new("Version", &["--version"]),
                RuntimeCheck::new(
                    "Images",
                    &["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"],
                ),
            ],
        },
        _ => RuntimeInfoTarget {
            title: "OrbStack",
            command: "orbctl",
            checks: vec![
                RuntimeCheck::new("Status", &["status"]),
                RuntimeCheck::new("Version", &["version"]),
            ],
        },
    };

    let child_path = build_child_path();
    let command_path = find_command_path(runtime.command, &child_path);
    let mut y = content_height - 58.0;
    add_label(
        parent,
        runtime.title,
        rect(34.0, y, width - 180.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    add_runtime_accent(parent, selected_nav, rect(24.0, y + 2.0, 4.0, 24.0), mtm);
    if selected_nav == NAV_APPLE_CONTAINER && command_path.is_some() {
        let open = add_button(
            parent,
            "Open Data Root",
            rect(width - 148.0, y - 4.0, 124.0, 28.0),
            handler,
            sel!(openAppleContainerDataRoot:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*open, setToolTip:
                &*NSString::from_str("Open Apple Container's local data directory in Finder")];
        }
    }
    y -= 42.0;

    let Some(command_path) = command_path else {
        let is_orbstack = selected_nav == NAV_ORBSTACK;
        let is_apple_container = selected_nav == NAV_APPLE_CONTAINER;
        let has_install_action = is_orbstack || is_apple_container;
        add_card(
            parent,
            rect(
                24.0,
                y - if has_install_action { 112.0 } else { 78.0 },
                width - 48.0,
                if has_install_action { 142.0 } else { 108.0 },
            ),
            mtm,
        );
        add_label(
            parent,
            RuntimeStatus::Unavailable.label(),
            rect(38.0, y + 8.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        let missing_detail = if is_orbstack {
            "OrbStack's CLI was not found. Open OrbStack once or use the Docker page with an OrbStack context."
                .to_string()
        } else if is_apple_container {
            "Apple Container is a separate Apple runtime and is not bundled with Cocoa-Way. Install Apple's latest official release, then return here to start it."
                .to_string()
        } else {
            format!("Command `{}` was not found in PATH.", runtime.command)
        };
        add_label(
            parent,
            &missing_detail,
            rect(38.0, y - 18.0, width - 76.0, 36.0),
            mtm,
            TextStyle::Body,
        );
        if is_orbstack {
            let open = add_button(
                parent,
                "Open OrbStack",
                rect(38.0, y - 76.0, 112.0, 28.0),
                handler,
                sel!(openOrbStackApp:),
                mtm,
            );
            unsafe {
                let _: () = msg_send![&*open, setToolTip:
                    &*NSString::from_str("Open OrbStack so its CLI and Docker endpoint become available")];
            }
        } else if is_apple_container {
            let download = add_button(
                parent,
                "Get Apple Container",
                rect(38.0, y - 76.0, 154.0, 28.0),
                handler,
                sel!(openAppleContainerReleases:),
                mtm,
            );
            unsafe {
                let _: () = msg_send![&*download, setToolTip:
                    &*NSString::from_str("Open Apple's official latest Apple Container release")];
            }
        }
        return;
    };

    let overview = runtime_overview(selected_nav, &command_path, &child_path);
    add_runtime_overview_card(parent, width, y, runtime.title, &overview, mtm);
    y -= 198.0;

    add_card(parent, rect(24.0, y - 58.0, width - 48.0, 88.0), mtm);
    add_label(
        parent,
        "Command",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        &command_path.display().to_string(),
        rect(38.0, y - 18.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Mono,
    );
    y -= 104.0;

    if selected_nav == NAV_APPLE_CONTAINER {
        add_apple_container_system_controls(parent, width, y, overview.status, handler, mtm);
        y -= 156.0;

        let compatibility = apple_container_compatibility(&command_path, &child_path);
        add_card(parent, rect(24.0, y - 160.0, width - 48.0, 190.0), mtm);
        add_label(
            parent,
            "Compatibility",
            rect(38.0, y + 8.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            &compatibility.summary,
            rect(38.0, y - 20.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Body,
        );
        add_label(
            parent,
            &format!(
                "CLI {} · API {} · system {}",
                compatibility.cli_version, compatibility.api_version, compatibility.system_status
            ),
            rect(38.0, y - 48.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Mono,
        );
        add_label(
            parent,
            &format!(
                "Published sockets: {} · resource JSON: {}",
                yes_no(compatibility.publish_socket),
                yes_no(compatibility.stats_json)
            ),
            rect(38.0, y - 76.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Caption,
        );
        add_label(
            parent,
            &compatibility.detail,
            rect(38.0, y - 142.0, width - 76.0, 54.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 206.0;

        let publish_socket_ready = compatibility.publish_socket;

        add_card(parent, rect(24.0, y - 150.0, width - 48.0, 180.0), mtm);
        add_label(
            parent,
            "GUI Transport",
            rect(38.0, y + 8.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        add_label(
            parent,
            if publish_socket_ready {
                "Transport V2 ready"
            } else {
                "Compatibility relay"
            },
            rect(38.0, y - 28.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Body,
        );
        add_label(
            parent,
            if publish_socket_ready {
                "Waypipe data uses Apple Container's published Unix socket. The stdio relay remains an automatic fallback."
            } else {
                "This Apple Container CLI has no --publish-socket support, so GUI launch uses the stdio compatibility relay."
            },
            rect(38.0, y - 112.0, width - 76.0, 70.0),
            mtm,
            TextStyle::Caption,
        );
        y -= 196.0;

        add_apple_container_inventory(parent, width, y, &command_path, &child_path, handler, mtm);
        y -= 340.0;
    } else if selected_nav == NAV_DOCKER {
        add_docker_context_inventory(parent, width, y, &child_path, handler, mtm);
        y -= 206.0;

        if let Some(orbctl_path) = find_command_path("orbctl", &child_path) {
            let running = orbstack_is_running(&orbctl_path, &child_path);
            let machine_height = add_orbstack_machine_inventory(
                parent,
                width,
                y,
                &orbctl_path,
                &child_path,
                running,
                handler,
                mtm,
            );
            y -= machine_height + 18.0;
        } else {
            add_card(parent, rect(24.0, y - 96.0, width - 48.0, 126.0), mtm);
            add_label(
                parent,
                "OrbStack Provider",
                rect(38.0, y + 8.0, width - 76.0, 20.0),
                mtm,
                TextStyle::Heading,
            );
            add_label(
                parent,
                "OrbStack is optional. Install or open it to expose Linux machines and its Docker context here.",
                rect(38.0, y - 42.0, width - 76.0, 42.0),
                mtm,
                TextStyle::Caption,
            );
            let open = add_button(
                parent,
                "Open OrbStack",
                rect(38.0, y - 82.0, 112.0, 28.0),
                handler,
                sel!(openOrbStackApp:),
                mtm,
            );
            unsafe {
                let _: () = msg_send![&*open, setToolTip:
                    &*NSString::from_str("Open OrbStack if it is installed")];
            }
            y -= 144.0;
        }

        add_docker_container_inventory(parent, width, y, &child_path, handler, mtm);
        y -= 318.0;
    } else if selected_nav == NAV_ORBSTACK {
        let running = orbstack_is_running(&command_path, &child_path);
        let machine_height = add_orbstack_machine_inventory(
            parent,
            width,
            y,
            &command_path,
            &child_path,
            running,
            handler,
            mtm,
        );
        y -= machine_height + 18.0;

        add_orbstack_docker_inventory(parent, width, y, &child_path, running, handler, mtm);
        y -= 316.0;
    }

    for check in runtime.checks {
        add_card(parent, rect(24.0, y - 88.0, width - 48.0, 118.0), mtm);
        add_label(
            parent,
            check.label,
            rect(38.0, y + 8.0, width - 76.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        let lines = command_preview_lines(&command_path, &child_path, check.args);
        let mut line_y = y - 18.0;
        for line in lines.iter().take(4) {
            add_label(
                parent,
                line,
                rect(38.0, line_y, width - 76.0, 18.0),
                mtm,
                TextStyle::Mono,
            );
            line_y -= 20.0;
        }
        y -= 134.0;
    }
}

fn add_apple_container_system_controls(
    parent: &NSView,
    width: f64,
    y: f64,
    runtime_status: RuntimeStatus,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(24.0, y - 110.0, width - 48.0, 140.0), mtm);
    add_label(
        parent,
        "System Controls",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Thin wrappers around Apple Container CLI commands.",
        rect(38.0, y - 24.0, width - 76.0, 24.0),
        mtm,
        TextStyle::Caption,
    );
    let start = add_button(
        parent,
        "Start System",
        rect(38.0, y - 58.0, 104.0, 28.0),
        handler,
        sel!(startAppleContainerSystem:),
        mtm,
    );
    let stop = add_button(
        parent,
        "Stop System",
        rect(152.0, y - 58.0, 104.0, 28.0),
        handler,
        sel!(stopAppleContainerSystem:),
        mtm,
    );
    let status = add_button(
        parent,
        "Copy Status",
        rect(38.0, y - 92.0, 104.0, 28.0),
        handler,
        sel!(copyContainerCommand:),
        mtm,
    );
    let releases = add_button(
        parent,
        "Latest Release",
        rect(152.0, y - 92.0, 104.0, 28.0),
        handler,
        sel!(openAppleContainerReleases:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*start, setToolTip:
            &*NSString::from_str("Run `container system start` asynchronously")];
        let _: () = msg_send![&*stop, setToolTip:
            &*NSString::from_str("Run `container system stop` asynchronously")];
        let _: () = msg_send![&*status, setTag: 0isize];
        let _: () = msg_send![&*status, setToolTip:
            &*NSString::from_str("Copy `container system status` to the clipboard")];
        let _: () = msg_send![&*releases, setToolTip:
            &*NSString::from_str("Open Apple's official latest Apple Container release")];
        let transitioning = matches!(
            runtime_status,
            RuntimeStatus::Starting | RuntimeStatus::Stopping
        );
        let can_start = !transitioning
            && matches!(
                runtime_status,
                RuntimeStatus::Unavailable | RuntimeStatus::Failed
            );
        let can_stop = !transitioning
            && matches!(
                runtime_status,
                RuntimeStatus::Ready | RuntimeStatus::Degraded
            );
        let _: () = msg_send![&*start, setEnabled: can_start];
        let _: () = msg_send![&*stop, setEnabled: can_stop];
        if transitioning {
            let _: () = msg_send![&*start, setTitle:
                &*NSString::from_str(runtime_status.label())];
        }
    }
}

fn add_apple_container_inventory(
    parent: &NSView,
    width: f64,
    y: f64,
    command_path: &std::path::Path,
    child_path: &str,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(24.0, y - 294.0, width - 48.0, 324.0), mtm);
    add_label(
        parent,
        "Containers",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Start, stop, or delete local instances without leaving Cocoa-Way.",
        rect(38.0, y - 40.0, width - 76.0, 30.0),
        mtm,
        TextStyle::Caption,
    );

    let rows = apple_container_rows(command_path, child_path);
    let mut row_y = y - 82.0;
    for row in rows.iter().take(4) {
        add_runtime_container_row(parent, width, row_y, "apple", row, handler, mtm);
        row_y -= if row.name.is_some() { 54.0 } else { 24.0 };
    }
}

fn apple_container_rows(
    command_path: &std::path::Path,
    child_path: &str,
) -> Vec<RuntimeContainerRow> {
    match run_ui_command(
        command_path,
        child_path,
        &["list", "--all"],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let mut rows = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .skip(1)
                .map(parse_apple_container_row)
                .collect::<Vec<_>>();
            if rows.is_empty() {
                rows.push(RuntimeContainerRow {
                    name: None,
                    label: "No local Apple containers".into(),
                    running: false,
                });
            }
            rows
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            vec![RuntimeContainerRow {
                name: None,
                label: stderr
                    .lines()
                    .next()
                    .filter(|line| !line.trim().is_empty())
                    .unwrap_or("container list failed")
                    .to_string(),
                running: false,
            }]
        }
        Err(error) => vec![RuntimeContainerRow {
            name: None,
            label: ui_command_error("container list failed", &error),
            running: false,
        }],
    }
}

fn parse_apple_container_row(line: &str) -> RuntimeContainerRow {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() < 5 {
        return RuntimeContainerRow {
            name: None,
            label: short_text(line, 96),
            running: false,
        };
    }

    let id = parts[0];
    let image = parts[1];
    let state = parts.get(4).copied().unwrap_or("unknown");
    if id == "buildkit" {
        return RuntimeContainerRow {
            name: None,
            label: format!("BuildKit helper  {}", state),
            running: state.eq_ignore_ascii_case("running"),
        };
    }

    RuntimeContainerRow {
        name: Some(id.to_string()),
        label: format!(
            "{}  {}  {}",
            short_text(id, 28),
            state,
            short_text(image, 38)
        ),
        running: state.eq_ignore_ascii_case("running"),
    }
}

fn add_docker_context_inventory(
    parent: &NSView,
    width: f64,
    y: f64,
    child_path: &str,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(24.0, y - 158.0, width - 48.0, 188.0), mtm);
    add_label(
        parent,
        "Docker Context",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Active Docker-compatible endpoint; OrbStack appears when its context is selected.",
        rect(38.0, y - 38.0, width - 76.0, 32.0),
        mtm,
        TextStyle::Caption,
    );

    let rows = docker_context_rows(child_path);
    let mut line_y = y - 76.0;
    for row in rows.iter().take(3) {
        let button_width = if row.name.is_some() && !row.current {
            58.0
        } else {
            0.0
        };
        add_label(
            parent,
            &row.label,
            rect(38.0, line_y, width - 84.0 - button_width, 22.0),
            mtm,
            TextStyle::Mono,
        );
        if let Some(name) = row.name.as_ref().filter(|_| !row.current) {
            let action_index = {
                let mut actions = DOCKER_CONTEXT_ACTIONS.lock().unwrap();
                let index = actions.len();
                actions.push(name.clone());
                index
            };
            let use_button = add_button(
                parent,
                "Use",
                rect(width - 94.0, line_y - 4.0, 52.0, 24.0),
                handler,
                sel!(useDockerContext:),
                mtm,
            );
            unsafe {
                let _: () = msg_send![&*use_button, setTag: action_index as isize];
                let _: () = msg_send![&*use_button, setToolTip:
                    &*NSString::from_str("Make this the active Docker context")];
            }
        }
        line_y -= 32.0;
    }
}

fn add_docker_container_inventory(
    parent: &NSView,
    width: f64,
    y: f64,
    child_path: &str,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(24.0, y - 262.0, width - 48.0, 292.0), mtm);
    add_label(
        parent,
        "Containers",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Stop running containers or remove stopped containers below.",
        rect(38.0, y - 28.0, width - 76.0, 18.0),
        mtm,
        TextStyle::Caption,
    );

    let rows = docker_container_rows(child_path);
    let mut row_y = y - 70.0;
    for row in rows.iter().take(4) {
        add_runtime_container_row(parent, width, row_y, "docker", row, handler, mtm);
        row_y -= if row.name.is_some() { 54.0 } else { 24.0 };
    }
}

fn add_orbstack_machine_inventory(
    parent: &NSView,
    width: f64,
    y: f64,
    command_path: &std::path::Path,
    child_path: &str,
    running: bool,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) -> f64 {
    let rows = orbstack_machine_rows(command_path, child_path, running);
    let visible_rows = rows.iter().take(3).collect::<Vec<_>>();
    let card_height = 142.0 + visible_rows.len() as f64 * 62.0;
    add_card(
        parent,
        rect(24.0, y - card_height + 30.0, width - 48.0, card_height),
        mtm,
    );
    add_label(
        parent,
        "Machines",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Manage OrbStack Linux machines independently from Docker-compatible containers.",
        rect(38.0, y - 38.0, width - 76.0, 32.0),
        mtm,
        TextStyle::Caption,
    );

    let start = add_button(
        parent,
        "Start OrbStack",
        rect(38.0, y - 76.0, 112.0, 28.0),
        handler,
        sel!(startOrbStack:),
        mtm,
    );
    let stop = add_button(
        parent,
        "Stop OrbStack",
        rect(160.0, y - 76.0, 112.0, 28.0),
        handler,
        sel!(stopOrbStack:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*start, setToolTip:
            &*NSString::from_str("Run `orbctl start` asynchronously")];
        let _: () = msg_send![&*stop, setToolTip:
            &*NSString::from_str("Run `orbctl stop` asynchronously")];
        let _: () = msg_send![&*start, setEnabled: !running];
        let _: () = msg_send![&*stop, setEnabled: running];
    }

    let mut row_y = y - 116.0;
    for row in visible_rows {
        add_orbstack_machine_row(parent, width, row_y, row, handler, mtm);
        row_y -= 62.0;
    }

    card_height
}

fn add_orbstack_machine_row(
    parent: &NSView,
    width: f64,
    y: f64,
    row: &OrbStackMachineRow,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(38.0, y - 42.0, width - 76.0, 54.0), mtm);
    let Some(name) = row.name.as_ref() else {
        add_label(
            parent,
            &row.label,
            rect(52.0, y - 4.0, width - 104.0, 18.0),
            mtm,
            TextStyle::Body,
        );
        add_label(
            parent,
            &short_text(
                &row.detail,
                chars_for_width(width - 104.0, TextStyle::Caption),
            ),
            rect(52.0, y - 26.0, width - 104.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        return;
    };

    let controls_width = 222.0;
    let text_width = (width - controls_width - 98.0).max(92.0);
    add_label(
        parent,
        &short_text(&row.label, chars_for_width(text_width, TextStyle::Body)),
        rect(52.0, y - 4.0, text_width, 18.0),
        mtm,
        TextStyle::Body,
    );
    add_label(
        parent,
        &short_text(&row.detail, chars_for_width(text_width, TextStyle::Caption)),
        rect(52.0, y - 26.0, text_width, 18.0),
        mtm,
        TextStyle::Caption,
    );

    let action_index = push_orbstack_machine_action(name);
    let primary = add_button(
        parent,
        if row.running { "Stop" } else { "Start" },
        rect(width - 246.0, y - 20.0, 64.0, 26.0),
        handler,
        if row.running {
            sel!(stopOrbStackMachine:)
        } else {
            sel!(startOrbStackMachine:)
        },
        mtm,
    );
    let shell = add_button(
        parent,
        "Shell",
        rect(width - 176.0, y - 20.0, 64.0, 26.0),
        handler,
        sel!(openOrbStackMachineTerminal:),
        mtm,
    );
    let delete = add_button(
        parent,
        "Delete",
        rect(width - 106.0, y - 20.0, 68.0, 26.0),
        handler,
        sel!(deleteOrbStackMachine:),
        mtm,
    );
    unsafe {
        for button in [&primary, &shell, &delete] {
            let _: () = msg_send![&**button, setTag: action_index as isize];
        }
        let _: () = msg_send![&*primary, setToolTip:
            &*NSString::from_str(if row.running { "Stop this OrbStack machine" } else { "Start this OrbStack machine" })];
        let _: () = msg_send![&*shell, setToolTip:
            &*NSString::from_str("Open an interactive shell for this machine in macOS Terminal")];
        let _: () = msg_send![&*delete, setToolTip:
            &*NSString::from_str("Permanently delete this machine after confirmation")];
    }
}

fn push_orbstack_machine_action(name: &str) -> usize {
    let mut actions = ORBSTACK_MACHINE_ACTIONS.lock().unwrap();
    let index = actions.len();
    actions.push(name.to_string());
    index
}

fn orbstack_machine_action_name(sender: &AnyObject) -> Option<String> {
    let tag: isize = unsafe { msg_send![sender, tag] };
    let name = ORBSTACK_MACHINE_ACTIONS
        .lock()
        .unwrap()
        .get(tag.max(0) as usize)
        .cloned();
    if name.is_none() {
        show_error_alert("OrbStack machine action no longer exists. Press Reload and try again.");
    }
    name
}

fn send_orbstack_machine_action(sender: &AnyObject, action: &str) {
    let Some(name) = orbstack_machine_action_name(sender) else {
        return;
    };
    send(CompositorMessage::RuntimeMachineAction {
        runtime: "orbstack".into(),
        name,
        action: action.into(),
    });
}

fn add_orbstack_docker_inventory(
    parent: &NSView,
    width: f64,
    y: f64,
    child_path: &str,
    running: bool,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(24.0, y - 266.0, width - 48.0, 296.0), mtm);
    add_label(
        parent,
        "Docker-compatible Containers",
        rect(38.0, y + 8.0, width - 76.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        if running {
            "Uses Docker CLI when the OrbStack context is active."
        } else {
            "OrbStack is stopped. Docker inventory is paused so Cocoa-Way does not wake it."
        },
        rect(38.0, y - 38.0, width - 76.0, 32.0),
        mtm,
        TextStyle::Caption,
    );

    if !running {
        add_label(
            parent,
            "Start OrbStack to inspect its Docker-compatible containers.",
            rect(38.0, y - 82.0, width - 76.0, 36.0),
            mtm,
            TextStyle::Body,
        );
        return;
    }

    let lines = docker_context_lines(child_path);
    let mut line_y = y - 76.0;
    for line in lines.iter().take(2) {
        add_label(
            parent,
            line,
            rect(38.0, line_y, width - 76.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        line_y -= 22.0;
    }
    let rows = docker_container_rows(child_path);
    line_y -= 12.0;
    for row in rows.iter().take(3) {
        add_runtime_container_row(parent, width, line_y, "orbstack", row, handler, mtm);
        line_y -= if row.name.is_some() { 54.0 } else { 24.0 };
    }
}

fn add_runtime_container_row(
    parent: &NSView,
    width: f64,
    y: f64,
    runtime: &str,
    row: &RuntimeContainerRow,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let Some(name) = row.name.as_ref() else {
        add_label(
            parent,
            &short_text(&row.label, chars_for_width(width - 76.0, TextStyle::Mono)),
            rect(38.0, y, width - 76.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        return;
    };

    let selected = {
        let mut selected = SELECTED_RUNTIME_CONTAINER.lock().unwrap();
        if let Some(selected) = selected
            .as_mut()
            .filter(|selected| selected.runtime == runtime && selected.name == name.as_str())
        {
            selected.label = row.label.clone();
            selected.running = row.running;
            true
        } else {
            false
        }
    };
    add_card(parent, rect(38.0, y - 36.0, width - 76.0, 46.0), mtm);
    if selected {
        add_runtime_accent(
            parent,
            runtime_nav(runtime),
            rect(38.0, y - 36.0, 4.0, 46.0),
            mtm,
        );
    }
    let text_width = width - 282.0;
    add_label(
        parent,
        &short_text(&row.label, chars_for_width(text_width, TextStyle::Mono)),
        rect(52.0, y - 6.0, text_width, 18.0),
        mtm,
        TextStyle::Mono,
    );

    let select_index = {
        let mut actions = RUNTIME_CONTAINER_SELECT_ACTIONS.lock().unwrap();
        let index = actions.len();
        actions.push(SelectedRuntimeContainer {
            runtime: runtime.to_string(),
            name: name.clone(),
            label: row.label.clone(),
            running: row.running,
        });
        index
    };
    add_hit_button(
        parent,
        rect(38.0, y - 36.0, (width - 272.0).max(96.0), 46.0),
        select_index,
        handler,
        sel!(selectRuntimeContainer:),
        mtm,
    );

    let action_index = push_runtime_container_action(runtime, name);
    let primary = add_button(
        parent,
        if row.running {
            "Stop Container"
        } else {
            "Start Container"
        },
        rect(width - 226.0, y - 12.0, 108.0, 24.0),
        handler,
        if row.running {
            sel!(stopRuntimeContainer:)
        } else {
            sel!(startRuntimeContainer:)
        },
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*primary, setTag: action_index as isize];
        let tooltip = if row.running {
            "Stop this runtime container"
        } else {
            "Start this stopped runtime container"
        };
        let _: () = msg_send![&*primary, setToolTip: &*NSString::from_str(tooltip)];
    }

    let action_index = push_runtime_container_action(runtime, name);
    let delete = add_button(
        parent,
        "Delete",
        rect(width - 108.0, y - 12.0, 74.0, 24.0),
        handler,
        sel!(deleteRuntimeContainer:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*delete, setTag: action_index as isize];
        let _: () = msg_send![&*delete, setEnabled: !row.running];
        let _: () = msg_send![&*delete, setToolTip:
        &*NSString::from_str(if row.running {
            "Stop this container before deleting it"
        } else {
            "Delete this stopped container after confirmation"
        })];
    }
}

fn push_runtime_container_action(runtime: &str, name: &str) -> usize {
    let mut actions = RUNTIME_CONTAINER_ACTIONS.lock().unwrap();
    let index = actions.len();
    actions.push((runtime.to_string(), name.to_string()));
    index
}

fn docker_context_lines(child_path: &str) -> Vec<String> {
    docker_context_rows(child_path)
        .into_iter()
        .map(|row| row.label)
        .collect()
}

struct DockerContextRow {
    name: Option<String>,
    label: String,
    current: bool,
}

fn docker_context_rows(child_path: &str) -> Vec<DockerContextRow> {
    let Some(path) = find_command_path("docker", child_path) else {
        return vec![DockerContextRow {
            name: None,
            label: "docker command not found".into(),
            current: false,
        }];
    };

    match run_ui_command(
        &path,
        child_path,
        &[
            "context",
            "ls",
            "--format",
            "{{.Name}}\t{{.Current}}\t{{.DockerEndpoint}}\t{{.Description}}",
        ],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let mut lines = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(parse_docker_context_line)
                .collect::<Vec<_>>();
            if lines.is_empty() {
                lines.push(DockerContextRow {
                    name: None,
                    label: "No Docker contexts found".into(),
                    current: false,
                });
            }
            lines
        }
        Ok(output) => vec![DockerContextRow {
            name: None,
            label: first_stderr_line(&output, "Docker context list failed"),
            current: false,
        }],
        Err(error) => vec![DockerContextRow {
            name: None,
            label: ui_command_error("Docker context list failed", &error),
            current: false,
        }],
    }
}

fn parse_docker_context_line(line: &str) -> Option<DockerContextRow> {
    let parts = line.split('\t').map(str::trim).collect::<Vec<_>>();
    let name = parts.first().copied().filter(|name| !name.is_empty())?;
    let current = parts.get(1).is_some_and(|value| *value == "true");
    let endpoint = parts.get(2).copied().unwrap_or_default();
    let description = parts.get(3).copied().unwrap_or_default();
    let detail = if description.is_empty() {
        endpoint
    } else {
        description
    };
    Some(DockerContextRow {
        name: Some(name.to_string()),
        label: format!(
            "{} {}  {}",
            if current { "*" } else { " " },
            name,
            short_text(detail, 52)
        ),
        current,
    })
}

struct RuntimeContainerRow {
    name: Option<String>,
    label: String,
    running: bool,
}

fn docker_container_rows(child_path: &str) -> Vec<RuntimeContainerRow> {
    let Some(path) = find_command_path("docker", child_path) else {
        return vec![RuntimeContainerRow {
            name: None,
            label: "docker command not found".into(),
            running: false,
        }];
    };

    match run_ui_command(
        &path,
        child_path,
        &[
            "ps",
            "-a",
            "--format",
            "{{.Names}}\t{{.State}}\t{{.Status}}\t{{.Image}}",
        ],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let mut rows = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(format_docker_container_row)
                .collect::<Vec<_>>();
            if rows.is_empty() {
                rows.push(RuntimeContainerRow {
                    name: None,
                    label: "No Docker containers to stop or delete".into(),
                    running: false,
                });
            }
            rows
        }
        Ok(output) => vec![RuntimeContainerRow {
            name: None,
            label: first_stderr_line(&output, "Docker container list failed"),
            running: false,
        }],
        Err(error) => vec![RuntimeContainerRow {
            name: None,
            label: ui_command_error("Docker container list failed", &error),
            running: false,
        }],
    }
}

fn orbstack_is_running(command_path: &std::path::Path, child_path: &str) -> bool {
    run_ui_command(
        command_path,
        child_path,
        &["status"],
        Duration::from_secs(2),
    )
    .ok()
    .filter(|output| output.status.success())
    .is_some_and(|output| {
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .eq_ignore_ascii_case("running")
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OrbStackMachineRow {
    name: Option<String>,
    label: String,
    detail: String,
    running: bool,
}

fn orbstack_machine_rows(
    command_path: &std::path::Path,
    child_path: &str,
    running: bool,
) -> Vec<OrbStackMachineRow> {
    if !running {
        return vec![OrbStackMachineRow {
            name: None,
            label: "OrbStack is stopped".into(),
            detail: "Start OrbStack to inspect its Linux machines.".into(),
            running: false,
        }];
    }

    match run_ui_command(
        command_path,
        child_path,
        &["list", "--format", "json"],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match parse_orbstack_machine_rows(&stdout) {
                Ok(rows) if !rows.is_empty() => rows,
                Ok(_) => vec![OrbStackMachineRow {
                    name: None,
                    label: "No OrbStack machines".into(),
                    detail: "Create or import a machine from OrbStack first.".into(),
                    running: false,
                }],
                Err(error) => vec![OrbStackMachineRow {
                    name: None,
                    label: "Machine list could not be parsed".into(),
                    detail: error,
                    running: false,
                }],
            }
        }
        Ok(output) => vec![OrbStackMachineRow {
            name: None,
            label: "Machine list failed".into(),
            detail: first_stderr_line(&output, "OrbStack returned an error"),
            running: false,
        }],
        Err(error) => vec![OrbStackMachineRow {
            name: None,
            label: if error == UI_COMMAND_LOADING {
                UI_COMMAND_LOADING.into()
            } else {
                "Machine list failed".into()
            },
            detail: if error == UI_COMMAND_LOADING {
                "Checking OrbStack in the background.".into()
            } else {
                error
            },
            running: false,
        }],
    }
}

fn parse_orbstack_machine_rows(json: &str) -> Result<Vec<OrbStackMachineRow>, String> {
    let values = serde_json::from_str::<Vec<serde_json::Value>>(json)
        .map_err(|error| format!("Invalid OrbStack JSON: {}", error))?;
    let mut rows = Vec::with_capacity(values.len());
    for value in values {
        let Some(name) = value.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let state = value
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let distro = value
            .pointer("/image/distro")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Linux");
        let version = value
            .pointer("/image/version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let arch = value
            .pointer("/image/arch")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let image = [distro, version]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let detail = [state, image.as_str(), arch]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        rows.push(OrbStackMachineRow {
            name: Some(name.to_string()),
            label: name.to_string(),
            detail,
            running: state.eq_ignore_ascii_case("running"),
        });
    }
    Ok(rows)
}

fn format_docker_container_row(line: &str) -> RuntimeContainerRow {
    let parts = line.split('\t').collect::<Vec<_>>();
    if parts.len() < 4 {
        return RuntimeContainerRow {
            name: None,
            label: short_text(line, 96),
            running: false,
        };
    }
    let name = parts[0].trim().to_string();
    let state = parts[1].trim();
    let status = parts[2].trim();
    let image = parts[3].trim();
    RuntimeContainerRow {
        name: Some(name.clone()),
        label: format!(
            "{}  {}  {}",
            short_text(&name, 26),
            short_text(status, 32),
            short_text(image, 30)
        ),
        running: matches!(state, "running" | "restarting" | "paused"),
    }
}

struct RuntimeInfoTarget {
    title: &'static str,
    command: &'static str,
    checks: Vec<RuntimeCheck>,
}

struct RuntimeOverview {
    status: RuntimeStatus,
    detail: String,
    version: String,
    resources: String,
    provider: String,
}

fn runtime_overview(
    selected_nav: usize,
    command_path: &std::path::Path,
    child_path: &str,
) -> RuntimeOverview {
    let runtime_key = match selected_nav {
        NAV_APPLE_CONTAINER => "apple",
        NAV_ORBSTACK => "orbstack",
        _ => "docker",
    };
    let operation = runtime_system_state(runtime_key);
    let version = command_preview_lines(command_path, child_path, &["--version"])
        .into_iter()
        .next()
        .unwrap_or_else(|| "Version unavailable".into());

    let (live_status, detail, resources, provider) = match selected_nav {
        NAV_APPLE_CONTAINER => {
            let compatibility = apple_container_compatibility(command_path, child_path);
            let system = compatibility.system_status.to_ascii_lowercase();
            let status = if system.contains("running") {
                RuntimeStatus::Ready
            } else if system.contains("stop") || system.contains("not running") {
                RuntimeStatus::Unavailable
            } else if system.contains("fail") || system.contains("error") {
                RuntimeStatus::Failed
            } else {
                RuntimeStatus::Degraded
            };
            let running = apple_container_rows(command_path, child_path)
                .iter()
                .filter(|row| row.name.is_some() && row.running)
                .count();
            (
                status,
                compatibility.detail,
                format_count(running, "running container"),
                if compatibility.publish_socket {
                    "GUI Transport V2".into()
                } else {
                    "Compatibility relay".into()
                },
            )
        }
        NAV_ORBSTACK => {
            let status_output = run_ui_command(
                command_path,
                child_path,
                &["status"],
                Duration::from_secs(2),
            );
            let (status, detail) = match status_output {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if text.eq_ignore_ascii_case("running") {
                        (
                            RuntimeStatus::Ready,
                            "OrbStack services are running.".into(),
                        )
                    } else {
                        (RuntimeStatus::Unavailable, format!("OrbStack is {text}."))
                    }
                }
                Ok(output) => (
                    RuntimeStatus::Failed,
                    first_stderr_line(&output, "OrbStack status failed"),
                ),
                Err(error) if error == UI_COMMAND_LOADING => (
                    RuntimeStatus::Degraded,
                    "Checking OrbStack status in the background.".into(),
                ),
                Err(error) => (RuntimeStatus::Failed, error),
            };
            let running =
                orbstack_machine_rows(command_path, child_path, status == RuntimeStatus::Ready)
                    .iter()
                    .filter(|row| row.name.is_some() && row.running)
                    .count();
            (
                status,
                detail,
                format_count(running, "running machine"),
                "OrbStack provider".into(),
            )
        }
        _ => {
            let info = run_ui_command(
                command_path,
                child_path,
                &["info", "--format", "{{.ServerVersion}}"],
                Duration::from_secs(2),
            );
            let (status, detail) = match info {
                Ok(output) if output.status.success() => (
                    RuntimeStatus::Ready,
                    format!(
                        "Connected to Docker Engine {}.",
                        String::from_utf8_lossy(&output.stdout).trim()
                    ),
                ),
                Ok(output) => (
                    RuntimeStatus::Unavailable,
                    first_stderr_line(&output, "Cannot connect to the Docker endpoint"),
                ),
                Err(error) if error == UI_COMMAND_LOADING => (
                    RuntimeStatus::Degraded,
                    "Checking the active Docker context in the background.".into(),
                ),
                Err(error) => (RuntimeStatus::Failed, error),
            };
            let running = docker_container_rows(child_path)
                .iter()
                .filter(|row| row.name.is_some() && row.running)
                .count();
            let context = docker_context_rows(child_path)
                .into_iter()
                .find(|row| row.current)
                .and_then(|row| row.name)
                .unwrap_or_else(|| "No active context".into());
            (
                status,
                detail,
                format_count(running, "running container"),
                format!("Context: {context}"),
            )
        }
    };

    let (status, detail) = operation
        .filter(|state| {
            matches!(
                state.status,
                RuntimeStatus::Starting | RuntimeStatus::Stopping
            )
        })
        .map(|state| (state.status, state.detail))
        .unwrap_or((live_status, detail));
    RuntimeOverview {
        status,
        detail,
        version,
        resources,
        provider,
    }
}

fn add_runtime_overview_card(
    parent: &NSView,
    width: f64,
    y: f64,
    title: &str,
    overview: &RuntimeOverview,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(24.0, y - 152.0, width - 48.0, 182.0), mtm);
    add_label(
        parent,
        title,
        rect(38.0, y + 8.0, width - 190.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        overview.status.label(),
        rect(width - 142.0, y + 8.0, 104.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        &overview.detail,
        rect(38.0, y - 40.0, width - 76.0, 42.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        &format!("Version\n{}", overview.version),
        rect(38.0, y - 104.0, (width - 92.0) * 0.52, 50.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        &format!("Resources\n{}\n{}", overview.resources, overview.provider),
        rect(width * 0.55, y - 126.0, width * 0.45 - 38.0, 72.0),
        mtm,
        TextStyle::Caption,
    );
}

struct RuntimeCheck {
    label: &'static str,
    args: &'static [&'static str],
}

impl RuntimeCheck {
    fn new(label: &'static str, args: &'static [&'static str]) -> Self {
        Self { label, args }
    }
}

fn run_ui_command(
    command: &std::path::Path,
    child_path: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Arc<Output>, String> {
    let key = ui_command_cache_key(command, child_path, args);
    let cached = UI_COMMAND_CACHE.lock().unwrap().get(&key).cloned();
    let stale = cached
        .as_ref()
        .is_none_or(|entry| entry.completed_at.elapsed() >= UI_COMMAND_CACHE_TTL);
    if stale {
        refresh_ui_command(
            key,
            command.to_path_buf(),
            child_path.to_string(),
            args.iter().map(|argument| argument.to_string()).collect(),
            timeout,
        );
    }
    cached.map_or_else(|| Err(UI_COMMAND_LOADING.into()), |entry| entry.result)
}

fn ui_command_cache_key(command: &std::path::Path, child_path: &str, args: &[&str]) -> String {
    let mut key = command.as_os_str().to_string_lossy().into_owned();
    key.push('\0');
    key.push_str(child_path);
    for argument in args {
        key.push('\0');
        key.push_str(argument);
    }
    key
}

fn refresh_ui_command(
    key: String,
    command: std::path::PathBuf,
    child_path: String,
    args: Vec<String>,
    timeout: Duration,
) {
    {
        let mut refreshing = UI_COMMAND_REFRESHING.lock().unwrap();
        if !refreshing.insert(key.clone()) {
            return;
        }
    }
    std::thread::spawn(move || {
        let result = execute_ui_command(&command, &child_path, &args, timeout).map(Arc::new);
        UI_COMMAND_CACHE.lock().unwrap().insert(
            key.clone(),
            UiCommandCacheEntry {
                completed_at: Instant::now(),
                result,
            },
        );
        UI_COMMAND_REFRESHING.lock().unwrap().remove(&key);
        send(CompositorMessage::ContainerModeCommandCacheUpdated);
    });
}

fn execute_ui_command(
    command: &std::path::Path,
    child_path: &str,
    args: &[String],
    timeout: Duration,
) -> Result<Output, String> {
    let mut child = Command::new(command)
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
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Command timed out after {}ms", timeout.as_millis()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn invalidate_ui_command_cache() {
    UI_COMMAND_CACHE.lock().unwrap().clear();
    *APPLE_COMPATIBILITY_CACHE.lock().unwrap() = None;
}

fn ui_command_error(prefix: &str, error: &str) -> String {
    if error == UI_COMMAND_LOADING {
        UI_COMMAND_LOADING.into()
    } else {
        format!("{}: {}", prefix, error)
    }
}

pub fn record_command_cache_updated() {
    *APPLE_COMPATIBILITY_CACHE.lock().unwrap() = None;
    let rebuilt = unsafe { rebuild_window_throttled(Duration::from_millis(100)) };
    if !rebuilt && !COMMAND_CACHE_REFRESH_PENDING.swap(true, Ordering::AcqRel) {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            send(CompositorMessage::ContainerModeCommandCacheRefreshDue);
        });
    }
}

pub fn record_command_cache_refresh_due() {
    COMMAND_CACHE_REFRESH_PENDING.store(false, Ordering::Release);
    *LAST_STREAM_REBUILD.lock().unwrap() = Some(Instant::now());
    unsafe {
        rebuild_window();
    }
}

fn apple_container_compatibility(
    command: &std::path::Path,
    child_path: &str,
) -> AppleContainerCompatibility {
    if let Some((checked_at, compatibility)) = APPLE_COMPATIBILITY_CACHE
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(checked_at, _)| checked_at.elapsed() < Duration::from_secs(5))
    {
        let _ = checked_at;
        return compatibility.clone();
    }

    let version_output =
        run_ui_command(command, child_path, &["--version"], Duration::from_secs(1))
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
    let cli_version = extract_version(&version_output).unwrap_or_else(|| "unknown".into());

    let status_json = run_ui_command(
        command,
        child_path,
        &["system", "status", "--format", "json"],
        Duration::from_secs(2),
    )
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok());
    let system_status = status_json
        .as_ref()
        .and_then(|value| value.get("status"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unavailable")
        .to_string();
    let api_version = status_json
        .as_ref()
        .and_then(|value| value.get("apiServerVersion"))
        .and_then(serde_json::Value::as_str)
        .and_then(extract_version)
        .unwrap_or_else(|| "unknown".into());

    let publish_socket = container_sessions::apple_publish_socket_supported(command, child_path);
    let stats_help = run_ui_command(
        command,
        child_path,
        &["stats", "--help"],
        Duration::from_secs(1),
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    .unwrap_or_default();
    let stats_json = stats_help.contains("--format") && stats_help.contains("--no-stream");

    let versions_match =
        cli_version == "unknown" || api_version == "unknown" || cli_version == api_version;
    let transport_v2_version = version_at_least(&cli_version, APPLE_CONTAINER_TRANSPORT_V2_MINIMUM);
    let security_current = version_at_least(&cli_version, APPLE_CONTAINER_SECURITY_BASELINE);
    let summary = if !versions_match {
        "Client/API version mismatch".into()
    } else if cli_version != "unknown" && !security_current {
        "Compatible; security update strongly recommended".into()
    } else if !publish_socket {
        "Legacy transport fallback required".into()
    } else if system_status != "running" {
        "Installed; management service is not running".into()
    } else {
        "Compatible with Cocoa-Way".into()
    };
    let detail = if !versions_match {
        "The CLI and API server differ. Restart or reinstall Apple Container before launching applications."
            .into()
    } else if cli_version != "unknown" && !security_current {
        format!(
            "Apple Container {} can run through Cocoa-Way, but 1.3.1 fixes multiple security vulnerabilities. Stop the service and run `/usr/local/bin/update-container.sh` before normal use.",
            cli_version
        )
    } else if !publish_socket {
        "Cocoa-Way can use its compatibility relay, but Transport V2 requires `container run --publish-socket`."
            .into()
    } else if transport_v2_version {
        "Apple Container 1.3.1+ satisfies Cocoa-Way's current security and Transport V2 compatibility baseline."
            .into()
    } else {
        "This CLI predates reliable non-root published sockets. Cocoa-Way will use its compatibility relay."
            .into()
    };

    let compatibility = AppleContainerCompatibility {
        cli_version,
        api_version,
        system_status,
        publish_socket,
        stats_json,
        summary,
        detail,
    };
    *APPLE_COMPATIBILITY_CACHE.lock().unwrap() = Some((Instant::now(), compatibility.clone()));
    compatibility
}

fn extract_version(text: &str) -> Option<String> {
    text.split(|character: char| character.is_whitespace() || character == '(')
        .map(|token| {
            token.trim_matches(|character: char| !character.is_ascii_digit() && character != '.')
        })
        .find(|token| {
            let parts = token.split('.').collect::<Vec<_>>();
            parts.len() >= 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

fn version_at_least(version: &str, minimum: (u64, u64, u64)) -> bool {
    let mut parts = version
        .split('.')
        .filter_map(|part| part.parse::<u64>().ok());
    let current = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    current >= minimum
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn command_preview_lines(
    command: &std::path::Path,
    child_path: &str,
    args: &[&str],
) -> Vec<String> {
    match run_ui_command(command, child_path, args, Duration::from_secs(2)) {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines = stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .take(8)
                .map(|line| line.to_string())
                .collect::<Vec<_>>();
            if lines.is_empty() {
                vec!["OK".into()]
            } else {
                lines
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            vec![
                stderr
                    .lines()
                    .next()
                    .filter(|line| !line.trim().is_empty())
                    .unwrap_or("Command failed")
                    .to_string(),
            ]
        }
        Err(error) => vec![ui_command_error("Command failed", &error)],
    }
}

fn resource_preview_lines(
    runtime_key: &str,
    resource: &str,
    action: &str,
    name: &str,
) -> Vec<String> {
    let child_path = build_child_path();
    let command_name = match runtime_key {
        "container" | "apple" | "apple container" => "container",
        "docker" => "docker",
        _ => return vec!["Unsupported runtime for inspect preview".into()],
    };
    let Some(command_path) = find_command_path(command_name, &child_path) else {
        return vec![format!("Command `{}` was not found in PATH.", command_name)];
    };
    let args = [resource, action, name];
    command_preview_lines(&command_path, &child_path, &args)
        .into_iter()
        .map(|line| short_text(&line, 96))
        .collect()
}

fn command_items() -> Vec<String> {
    let config_path = container_sessions::config_path();
    vec![
        format!("container system status"),
        format!("container image list"),
        smoke_image_build_command(),
        format!("container image pull docker.io/library/alpine:3.20"),
        format!("container image load --input /tmp/cocoa-way-niri.tar"),
        format!("docker image ls"),
        format!(
            "docker buildx build -f examples/container-images/Containerfile.niri --output type=oci,dest=/tmp/cocoa-way-niri.tar ."
        ),
        format!(
            "open '{}'",
            apple_container_data_root().replace('\'', "'\\''")
        ),
        format!("open -R {}", config_path.display()),
    ]
}

fn add_commands_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let commands = command_items();
    let mut y = content_height - 58.0;
    add_label(
        parent,
        "Useful Commands",
        rect(34.0, y, width - 68.0, 24.0),
        mtm,
        TextStyle::Title,
    );
    y -= 42.0;
    for (index, command) in commands.iter().enumerate() {
        add_card(parent, rect(24.0, y - 18.0, width - 48.0, 48.0), mtm);
        add_label(
            parent,
            command,
            rect(38.0, y, width - 142.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        let copy = add_button(
            parent,
            "Copy",
            rect(width - 86.0, y - 6.0, 62.0, 28.0),
            handler,
            sel!(copyContainerCommand:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*copy, setTag: index as isize];
            let _: () = msg_send![&*copy, setToolTip:
                &*NSString::from_str("Copy this command to the clipboard")];
        }
        y -= 62.0;
    }
}

struct ImageInventory {
    runtime: &'static str,
    runtime_key: &'static str,
    rows: Vec<ImageRow>,
}

struct ImageRow {
    label: String,
    reference: Option<String>,
}

struct VolumeInventory {
    runtime: &'static str,
    runtime_key: &'static str,
    rows: Vec<VolumeRow>,
}

struct VolumeRow {
    label: String,
    name: Option<String>,
}

fn image_inventories() -> Vec<ImageInventory> {
    if let Some((task, detail)) = image_task_active() {
        let mut rows = vec![ImageRow::message(task)];
        if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
            rows.push(ImageRow::message(detail));
        }
        return vec![ImageInventory {
            runtime: "Apple Container",
            runtime_key: "container",
            rows,
        }];
    }

    let child_path = build_child_path();
    vec![
        ImageInventory {
            runtime: "Apple Container",
            runtime_key: "container",
            rows: apple_container_image_rows(&child_path),
        },
        ImageInventory {
            runtime: "Docker-compatible Context",
            runtime_key: "docker",
            rows: docker_image_rows(&child_path),
        },
    ]
}

fn apple_registry_login_summary(child_path: &str) -> String {
    let Some(path) = find_command_path("container", child_path) else {
        return "Registry login unavailable: Apple Container is not installed.".into();
    };
    let output = run_ui_command(
        &path,
        child_path,
        &["registry", "list", "--quiet"],
        Duration::from_secs(2),
    );
    let Ok(output) = output else {
        return UI_COMMAND_LOADING.into();
    };
    if !output.status.success() {
        return "Registry login status unavailable.".into();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let registries = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>();
    if registries.is_empty() {
        "Public pulls ready; no private registry login saved.".into()
    } else {
        format!("Signed in: {}", registries.join(", "))
    }
}

fn volume_inventories() -> Vec<VolumeInventory> {
    let child_path = build_child_path();
    vec![
        VolumeInventory {
            runtime: "Apple Container",
            runtime_key: "container",
            rows: apple_container_volume_rows(&child_path),
        },
        VolumeInventory {
            runtime: "Docker-compatible Context",
            runtime_key: "docker",
            rows: docker_volume_rows(&child_path),
        },
    ]
}

fn volume_usage(runtime_key: &str, volume_name: &str) -> VolumeUsage {
    let referenced_profiles = container_sessions::load_sessions()
        .into_iter()
        .filter(|session| {
            runtime_key_matches(runtime_key, &session.runtime)
                && session
                    .mounts
                    .iter()
                    .any(|mount| mount_references_volume(mount, volume_name))
        })
        .map(|session| session.name)
        .collect::<Vec<_>>();
    let child_path = build_child_path();
    let (command, args) = if runtime_key == "docker" {
        let filter = format!("volume={volume_name}");
        (
            "docker",
            vec![
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                filter,
                "--format".into(),
                "{{.Names}}".into(),
            ],
        )
    } else {
        (
            "container",
            vec![
                "list".into(),
                "--all".into(),
                "--format".into(),
                "json".into(),
            ],
        )
    };
    let Some(command_path) = find_command_path(command, &child_path) else {
        return VolumeUsage {
            referenced_profiles,
            error: Some(format!("{} is not installed", runtime_label(runtime_key))),
            ..VolumeUsage::default()
        };
    };
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    match run_ui_command(
        &command_path,
        &child_path,
        &arg_refs,
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let parsed = if runtime_key == "docker" {
                Ok(String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>())
            } else {
                parse_apple_volume_mounts(&String::from_utf8_lossy(&output.stdout), volume_name)
            };
            match parsed {
                Ok(mut mounted_containers) => {
                    mounted_containers.sort();
                    mounted_containers.dedup();
                    VolumeUsage {
                        referenced_profiles,
                        mounted_containers,
                        ..VolumeUsage::default()
                    }
                }
                Err(error) => VolumeUsage {
                    referenced_profiles,
                    error: Some(error),
                    ..VolumeUsage::default()
                },
            }
        }
        Ok(output) => VolumeUsage {
            referenced_profiles,
            error: Some(first_stderr_line(&output, "Volume usage check failed")),
            ..VolumeUsage::default()
        },
        Err(error) if error == UI_COMMAND_LOADING => VolumeUsage {
            referenced_profiles,
            loading: true,
            ..VolumeUsage::default()
        },
        Err(error) => VolumeUsage {
            referenced_profiles,
            error: Some(ui_command_error("Volume usage check failed", &error)),
            ..VolumeUsage::default()
        },
    }
}

fn mount_references_volume(mount: &str, volume_name: &str) -> bool {
    let mount = mount.trim();
    if mount == volume_name
        || mount
            .strip_prefix(volume_name)
            .is_some_and(|rest| rest.starts_with(':'))
    {
        return true;
    }
    mount.split(',').any(|part| {
        let Some((key, value)) = part.trim().split_once('=') else {
            return false;
        };
        matches!(key.trim(), "source" | "src" | "volume" | "name") && value.trim() == volume_name
    })
}

fn parse_apple_volume_mounts(json: &str, volume_name: &str) -> Result<Vec<String>, String> {
    let containers = serde_json::from_str::<Vec<serde_json::Value>>(json)
        .map_err(|error| format!("Apple Container returned invalid container JSON: {error}"))?;
    Ok(containers
        .into_iter()
        .filter_map(|container| {
            let configuration = container.get("configuration")?;
            let mounted = configuration
                .get("mounts")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|mounts| {
                    mounts.iter().any(|mount| {
                        mount.get("source").and_then(serde_json::Value::as_str) == Some(volume_name)
                    })
                });
            mounted.then(|| {
                container
                    .get("id")
                    .or_else(|| configuration.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Unknown container")
                    .to_string()
            })
        })
        .collect())
}

fn volume_metadata(label: &str, name: &str) -> String {
    label
        .strip_prefix(name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Managed volume")
        .to_string()
}

fn volume_inspect_metadata(runtime_key: &str, name: &str, label: &str) -> VolumeMetadata {
    let fallback_kind = volume_metadata(label, name);
    let child_path = build_child_path();
    let command = if runtime_key == "docker" {
        "docker"
    } else {
        "container"
    };
    let Some(command_path) = find_command_path(command, &child_path) else {
        return VolumeMetadata {
            kind: fallback_kind,
            size: "Unavailable".into(),
            created: "Unavailable".into(),
        };
    };
    let args = ["volume", "inspect", name];
    match run_ui_command(&command_path, &child_path, &args, Duration::from_secs(2)) {
        Ok(output) if output.status.success() => {
            parse_volume_inspect_metadata(&output.stdout, &fallback_kind).unwrap_or(
                VolumeMetadata {
                    kind: fallback_kind,
                    size: "Unknown".into(),
                    created: "Unknown".into(),
                },
            )
        }
        Err(error) if error == UI_COMMAND_LOADING => VolumeMetadata {
            kind: fallback_kind,
            size: "Loading...".into(),
            created: "Loading...".into(),
        },
        _ => VolumeMetadata {
            kind: fallback_kind,
            size: "Unavailable".into(),
            created: "Unavailable".into(),
        },
    }
}

fn parse_volume_inspect_metadata(bytes: &[u8], fallback_kind: &str) -> Option<VolumeMetadata> {
    let root = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    let value = root
        .as_array()
        .and_then(|values| values.first())
        .unwrap_or(&root);
    let string_field = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
            .map(str::to_string)
    };
    let kind = string_field(&["Driver", "driver", "Type", "type"])
        .unwrap_or_else(|| fallback_kind.to_string());
    let size = ["Size", "size", "sizeInBytes", "size_in_bytes"]
        .iter()
        .find_map(|key| value.get(*key))
        .and_then(|value| {
            value
                .as_u64()
                .map(format_byte_count)
                .or_else(|| value.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "Unknown".into());
    let created = string_field(&["CreatedAt", "createdAt", "creationDate", "created"])
        .unwrap_or_else(|| "Unknown".into());
    Some(VolumeMetadata {
        kind,
        size,
        created,
    })
}

fn format_byte_count(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn apple_container_image_rows(child_path: &str) -> Vec<ImageRow> {
    let Some(path) = find_command_path("container", child_path) else {
        return vec![ImageRow::message("container not installed")];
    };

    match run_ui_command(
        &path,
        child_path,
        &["image", "list"],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let rows = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(parse_apple_container_image_line)
                .collect::<Vec<_>>();
            if rows.is_empty() {
                vec![ImageRow::message("No local images found")]
            } else {
                rows
            }
        }
        Ok(output) => vec![ImageRow::message(first_stderr_line(
            &output,
            "Image list failed",
        ))],
        Err(error) => vec![ImageRow::message(ui_command_error(
            "Image list failed",
            &error,
        ))],
    }
}

fn apple_container_volume_rows(child_path: &str) -> Vec<VolumeRow> {
    let Some(path) = find_command_path("container", child_path) else {
        return vec![VolumeRow::message("container not installed")];
    };

    match run_ui_command(
        &path,
        child_path,
        &["volume", "list"],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let rows = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(parse_volume_line)
                .collect::<Vec<_>>();
            if rows.is_empty() {
                vec![VolumeRow::message("No local volumes found")]
            } else {
                rows
            }
        }
        Ok(output) => vec![VolumeRow::message(first_stderr_line(
            &output,
            "Volume list failed",
        ))],
        Err(error) => vec![VolumeRow::message(ui_command_error(
            "Volume list failed",
            &error,
        ))],
    }
}

fn docker_image_rows(child_path: &str) -> Vec<ImageRow> {
    let Some(path) = find_command_path("docker", child_path) else {
        return vec![ImageRow::message("docker not installed")];
    };
    match run_ui_command(
        &path,
        child_path,
        &[
            "image",
            "ls",
            "--format",
            "{{.Repository}}:{{.Tag}}\t{{.ID}}\t{{.Size}}",
        ],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let rows = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(parse_docker_image_line)
                .collect::<Vec<_>>();
            if rows.is_empty() {
                vec![ImageRow::message("No Docker-compatible images found")]
            } else {
                rows
            }
        }
        Ok(output) => vec![ImageRow::message(first_stderr_line(
            &output,
            "Docker image list failed",
        ))],
        Err(error) => vec![ImageRow::message(ui_command_error(
            "Docker image list failed",
            &error,
        ))],
    }
}

fn docker_volume_rows(child_path: &str) -> Vec<VolumeRow> {
    let Some(path) = find_command_path("docker", child_path) else {
        return vec![VolumeRow::message("docker not installed")];
    };
    match run_ui_command(
        &path,
        child_path,
        &["volume", "ls", "--format", "{{.Name}}\t{{.Driver}}"],
        Duration::from_secs(2),
    ) {
        Ok(output) if output.status.success() => {
            let rows = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(parse_volume_line)
                .collect::<Vec<_>>();
            if rows.is_empty() {
                vec![VolumeRow::message("No Docker-compatible volumes found")]
            } else {
                rows
            }
        }
        Ok(output) => vec![VolumeRow::message(first_stderr_line(
            &output,
            "Docker volume list failed",
        ))],
        Err(error) => vec![VolumeRow::message(ui_command_error(
            "Docker volume list failed",
            &error,
        ))],
    }
}

fn parse_volume_line(line: &str) -> Option<VolumeRow> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let name = trimmed.split_whitespace().next()?;
    if name.eq_ignore_ascii_case("name") {
        return None;
    }
    Some(VolumeRow {
        label: trimmed.to_string(),
        name: Some(name.to_string()),
    })
}

fn parse_apple_container_image_line(line: &str) -> Option<ImageRow> {
    let columns = line.split_whitespace().collect::<Vec<_>>();
    if columns.is_empty() || columns[0].eq_ignore_ascii_case("name") {
        return None;
    }

    let name = columns[0];
    let tag = columns.get(1).copied().unwrap_or_default();
    let digest = columns.get(2).copied().unwrap_or_default();
    let reference = if tag.is_empty() || tag == "<none>" {
        name.to_string()
    } else {
        format!("{}:{}", name, tag)
    };
    let label = if digest.is_empty() {
        reference.clone()
    } else {
        format!("{}    {}", reference, digest)
    };

    Some(ImageRow {
        label,
        reference: Some(reference),
    })
}

fn parse_docker_image_line(line: &str) -> Option<ImageRow> {
    let columns = line.split('\t').map(str::trim).collect::<Vec<_>>();
    let tagged_reference = columns.first().copied().filter(|value| !value.is_empty())?;
    let id = columns.get(1).copied().unwrap_or_default();
    let size = columns.get(2).copied().unwrap_or_default();
    let reference = if tagged_reference.starts_with("<none>:") {
        id.to_string()
    } else {
        tagged_reference.to_string()
    };
    if reference.is_empty() {
        return None;
    }
    let metadata = [id, size]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("    ");
    Some(ImageRow {
        label: if metadata.is_empty() {
            reference.clone()
        } else {
            format!("{}    {}", reference, metadata)
        },
        reference: Some(reference),
    })
}

impl ImageRow {
    fn message(message: impl Into<String>) -> Self {
        Self {
            label: message.into(),
            reference: None,
        }
    }
}

fn runtime_key_matches(runtime_key: &str, session_runtime: &str) -> bool {
    let normalized_session_runtime = session_runtime.trim().to_ascii_lowercase();
    match runtime_key.trim().to_ascii_lowercase().as_str() {
        "container" | "apple" | "apple container" => !matches!(
            normalized_session_runtime.as_str(),
            "docker" | "orb" | "orbstack"
        ),
        "docker" => matches!(
            normalized_session_runtime.as_str(),
            "docker" | "orb" | "orbstack"
        ),
        other => other == normalized_session_runtime,
    }
}

fn image_reference_has_tag(reference: &str) -> bool {
    let last_slash = reference.rfind('/');
    reference
        .rfind(':')
        .is_some_and(|colon| last_slash.is_none_or(|slash| colon > slash))
}

fn split_image_reference(reference: &str) -> (&str, &str) {
    let last_slash = reference.rfind('/');
    if let Some(colon) = reference
        .rfind(':')
        .filter(|colon| last_slash.is_none_or(|slash| *colon > slash))
    {
        (&reference[..colon], &reference[colon + 1..])
    } else {
        (reference, "<none>")
    }
}

fn image_id_from_label(label: &str, reference: &str) -> Option<String> {
    label
        .strip_prefix(reference)
        .and_then(|metadata| metadata.split_whitespace().next())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn image_references_for_id(runtime_key: &str, image_id: &str) -> Vec<String> {
    let mut references = image_inventories()
        .into_iter()
        .filter(|inventory| inventory.runtime_key == runtime_key)
        .flat_map(|inventory| inventory.rows)
        .filter_map(|row| {
            let reference = row.reference?;
            (image_id_from_label(&row.label, &reference).as_deref() == Some(image_id))
                .then_some(reference)
        })
        .collect::<Vec<_>>();
    references.sort();
    references.dedup();
    references
}

impl VolumeRow {
    fn message(message: impl Into<String>) -> Self {
        Self {
            label: message.into(),
            name: None,
        }
    }
}

fn first_stderr_line(output: &Output, fallback: &str) -> String {
    let line = String::from_utf8_lossy(&output.stderr)
        .lines()
        .next()
        .filter(|line| !line.trim().is_empty())
        .unwrap_or(fallback)
        .to_string();
    if line.contains("Cannot connect to the Docker daemon") {
        "Docker daemon is offline. Start Docker or OrbStack, then Reload.".into()
    } else {
        short_text(&line, 82)
    }
}

fn add_detail_panel(
    parent: &NSView,
    x: f64,
    _y: f64,
    width: f64,
    height: f64,
    selected_tab: usize,
    selected_nav: usize,
    selected_session: Option<(usize, &ContainerSession)>,
    scroll_key: String,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let compact_toolbar = width < 560.0;
    if matches!(selected_nav, NAV_SESSIONS | NAV_RUNNING) {
        let tab_x = if compact_toolbar {
            x + 122.0
        } else {
            x + 144.0
        };
        let max_tab_w = (x + width - tab_x - 24.0).max(190.0);
        let tab_w = max_tab_w.min(440.0);
        let terminal = add_button(
            parent,
            "New Terminal",
            rect(
                if compact_toolbar { x + 12.0 } else { x + 22.0 },
                height - 47.0,
                if compact_toolbar { 104.0 } else { 112.0 },
                30.0,
            ),
            handler,
            sel!(openContainerTerminal:),
            mtm,
        );
        unsafe {
            if let Some((index, _)) =
                selected_session.filter(|(index, _)| active_session(*index).is_some())
            {
                let _: () = msg_send![&*terminal, setTag: index as isize];
                let _: () = msg_send![&*terminal, setToolTip:
                    &*NSString::from_str("Open a terminal in the running application instance")];
            } else {
                let _: () = msg_send![&*terminal, setEnabled: false];
                let _: () = msg_send![&*terminal, setToolTip:
                    &*NSString::from_str("Launch an application instance before opening its terminal")];
            }
        }
        add_tab_bar(
            parent,
            tab_x,
            height - 50.0,
            tab_w,
            selected_tab,
            handler,
            mtm,
        );
    } else {
        add_label(
            parent,
            nav_title(selected_nav),
            rect(x + 28.0, height - 44.0, width - 56.0, 26.0),
            mtm,
            TextStyle::Title,
        );
    }
    add_separator(parent, rect(x, height - 64.0, width, 1.0), mtm);

    let summary_height = 72.0;
    let scroll_height = (height - 64.0 - summary_height).max(240.0);
    let scroll = unsafe {
        NSScrollView::initWithFrame(
            mtm.alloc::<NSScrollView>(),
            rect(x, summary_height, width, scroll_height),
        )
    };
    unsafe {
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
    }
    let selected_runtime = SELECTED_RUNTIME_CONTAINER
        .lock()
        .unwrap()
        .clone()
        .filter(|container| runtime_nav(&container.runtime) == selected_nav);
    let document_width = (width - 14.0).max(320.0);
    let document_height = if selected_session.is_some() {
        // Application details are intentionally split into independent cards. Keep the
        // document tall enough for narrow-window wrapping and the eight-stage task view.
        2200.0_f64.max(scroll_height)
    } else if selected_runtime.is_some() {
        860.0_f64.max(scroll_height)
    } else if matches!(selected_nav, NAV_IMAGES | NAV_VOLUMES) {
        900.0_f64.max(scroll_height)
    } else {
        scroll_height
    };
    let document: Retained<NSView> = unsafe {
        msg_send_id![
            mtm.alloc::<NSView>(),
            initWithFrame: rect(0.0, 0.0, document_width, document_height)
        ]
    };

    if let Some((index, session)) = selected_session {
        add_session_detail(
            &document,
            index,
            session,
            session_state(index).as_ref(),
            0.0,
            0.0,
            document_width,
            document_height,
            selected_tab,
            handler,
            mtm,
        );
    } else if matches!(selected_nav, NAV_SESSIONS | NAV_RUNNING) {
        let content_x = 42.0;
        let content_y = document_height * 0.52;
        add_label(
            &document,
            "No Selection",
            rect(content_x, content_y, document_width - 84.0, 42.0),
            mtm,
            TextStyle::Hero,
        );
        add_label(
            &document,
            detail_empty_message(selected_tab),
            rect(content_x, content_y - 40.0, document_width - 84.0, 40.0),
            mtm,
            TextStyle::Body,
        );
    } else if selected_nav == NAV_IMAGES {
        if let Some(image) = SELECTED_IMAGE.lock().unwrap().clone() {
            add_image_detail(
                &document,
                &image,
                0.0,
                0.0,
                document_width,
                document_height,
                handler,
                mtm,
            );
        } else {
            add_section_detail(
                &document,
                nav_title(selected_nav),
                0.0,
                0.0,
                document_width,
                document_height,
                mtm,
            );
        }
    } else if selected_nav == NAV_VOLUMES {
        if let Some(volume) = SELECTED_VOLUME.lock().unwrap().clone() {
            add_volume_detail(
                &document,
                &volume,
                0.0,
                0.0,
                document_width,
                document_height,
                handler,
                mtm,
            );
        } else {
            add_section_detail(
                &document,
                nav_title(selected_nav),
                0.0,
                0.0,
                document_width,
                document_height,
                mtm,
            );
        }
    } else if let Some(container) = selected_runtime {
        add_runtime_container_detail(
            &document,
            &container,
            0.0,
            0.0,
            document_width,
            document_height,
            handler,
            mtm,
        );
    } else {
        add_section_detail(
            &document,
            nav_title(selected_nav),
            0.0,
            0.0,
            document_width,
            document_height,
            mtm,
        );
    }
    unsafe {
        scroll.setDocumentView(Some(&document));
        let clip_view: Retained<AnyObject> = msg_send_id![&*scroll, contentView];
        let top_y = (document_height - scroll_height).max(0.0);
        let saved_y = saved_scroll_position(&scroll_key, top_y).clamp(0.0, top_y);
        let _: () = msg_send![&*clip_view, scrollToPoint: NSPoint { x: 0.0, y: saved_y }];
        let _: () = msg_send![&*scroll, reflectScrolledClipView: &*clip_view];
        parent.addSubview(&scroll);
        *DETAIL_SCROLL_VIEW.lock().unwrap() = Some(TrackedScrollView {
            pointer: (&*scroll as *const NSScrollView) as usize,
            key: scroll_key,
        });
    }
    add_separator(parent, rect(x, summary_height, width, 1.0), mtm);
    add_runtime_summary(parent, x + 28.0, 4.0, width - 56.0, mtm);
}

fn detail_scroll_key(
    selected_nav: usize,
    selected_tab: usize,
    selected_session: Option<usize>,
) -> String {
    if let Some(index) = selected_session {
        return format!("detail:{selected_nav}:application:{index}:tab:{selected_tab}");
    }
    if selected_nav == NAV_IMAGES {
        return SELECTED_IMAGE
            .lock()
            .unwrap()
            .as_ref()
            .map(|image| format!("detail:{selected_nav}:image:{}", image.reference))
            .unwrap_or_else(|| format!("detail:{selected_nav}:empty"));
    }
    if selected_nav == NAV_VOLUMES {
        return SELECTED_VOLUME
            .lock()
            .unwrap()
            .as_ref()
            .map(|volume| format!("detail:{selected_nav}:volume:{}", volume.name))
            .unwrap_or_else(|| format!("detail:{selected_nav}:empty"));
    }
    if let Some(container) = SELECTED_RUNTIME_CONTAINER.lock().unwrap().as_ref() {
        if runtime_nav(&container.runtime) == selected_nav {
            return format!("detail:{selected_nav}:container:{}", container.name);
        }
    }
    format!("detail:{selected_nav}:empty")
}

unsafe fn capture_tracked_scroll_position(slot: &Mutex<Option<TrackedScrollView>>) {
    let Some(tracked) = slot.lock().unwrap().clone() else {
        return;
    };
    let scroll = unsafe { &*(tracked.pointer as *const NSScrollView) };
    let clip_view: Retained<AnyObject> = unsafe { msg_send_id![scroll, contentView] };
    let bounds: NSRect = unsafe { msg_send![&*clip_view, bounds] };
    SCROLL_POSITIONS
        .lock()
        .unwrap()
        .insert(tracked.key, bounds.origin.y);
}

fn saved_scroll_position(key: &str, default_y: f64) -> f64 {
    SCROLL_POSITIONS
        .lock()
        .unwrap()
        .get(key)
        .copied()
        .unwrap_or(default_y)
}

fn add_session_detail(
    parent: &NSView,
    index: usize,
    session: &ContainerSession,
    state: Option<&SessionState>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    selected_tab: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let content_x = x + 42.0;
    let header_y = y + height - 124.0;
    add_label(
        parent,
        &session.name,
        rect(content_x, header_y, width - 84.0, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        &format!(
            "{} application backed by {}",
            runtime_label(&session.runtime),
            session.image
        ),
        rect(content_x, header_y - 28.0, width - 84.0, 20.0),
        mtm,
        TextStyle::Body,
    );
    let actions_y = header_y - 72.0;
    let compact_actions = width < 520.0;
    let secondary_y = actions_y - 40.0;
    let active = active_session(index);
    let missing_image = state.is_some_and(is_missing_image_state);
    let transport_blocked = session_has_apple_transport_block(session);
    let stop_enabled = active.is_some() || session_can_stop(state);
    let launch_busy = active.is_some() || session_is_launch_busy(state);
    let check = add_button(
        parent,
        "Run Health Check",
        rect(content_x, actions_y, 128.0, 30.0),
        handler,
        sel!(checkContainerSession:),
        mtm,
    );
    let primary_label = if launch_busy {
        state.map(session_state_label).unwrap_or("Running")
    } else if transport_blocked {
        "Blocked"
    } else if missing_image {
        if is_smoke_image_reference(&session.image) {
            "Build"
        } else if is_local_image_reference(&session.image) {
            "Load OCI"
        } else {
            "Pull"
        }
    } else {
        "Launch"
    };
    let primary_selector = if launch_busy || transport_blocked {
        sel!(checkContainerSession:)
    } else if missing_image {
        if is_smoke_image_reference(&session.image) {
            sel!(buildSmokeContainerSessionImage:)
        } else if is_local_image_reference(&session.image) {
            sel!(loadContainerSessionImage:)
        } else {
            sel!(pullContainerSessionImage:)
        }
    } else {
        sel!(launchContainerSession:)
    };
    let primary_tooltip = if launch_busy {
        "This application already has an active instance. Stop it before launching again."
    } else if transport_blocked {
        "Apple Container GUI relay is currently unavailable"
    } else if missing_image {
        if is_smoke_image_reference(&session.image) {
            "Build the bundled example image with Apple Container before launching"
        } else if is_local_image_reference(&session.image) {
            "Load an OCI archive into Apple Container before launching"
        } else {
            "Pull the missing image before launching"
        }
    } else {
        "Launch this application"
    };
    let primary = add_button(
        parent,
        primary_label,
        rect(content_x + 138.0, actions_y, 88.0, 30.0),
        handler,
        primary_selector,
        mtm,
    );
    let force_stop = state.is_some_and(|state| state.force_stop_available);
    let stop = add_button(
        parent,
        if force_stop { "Force Stop" } else { "Stop" },
        rect(content_x + 236.0, actions_y, 92.0, 30.0),
        handler,
        if force_stop {
            sel!(forceStopContainerSession:)
        } else {
            sel!(stopContainerSession:)
        },
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*check, setTag: index as isize];
        let _: () = msg_send![&*check, setToolTip:
            &*NSString::from_str("Validate the profile, runtime, image, command, and transport without launching")];
        let _: () = msg_send![&*primary, setTag: index as isize];
        let _: () = msg_send![&*primary, setToolTip:
            &*NSString::from_str(primary_tooltip)];
        if launch_busy || transport_blocked {
            let _: () = msg_send![&*primary, setEnabled: false];
        }
        let _: () = msg_send![&*stop, setTag: index as isize];
        let _: () = msg_send![&*stop, setToolTip:
        &*NSString::from_str(if force_stop {
            "Immediately terminate the application after graceful stop timed out"
        } else {
            "Ask the running application instance to exit gracefully"
        })];
        if !stop_enabled {
            let _: () = msg_send![&*stop, setEnabled: false];
        }
    }

    let edit = add_button(
        parent,
        "Edit Profile",
        rect(
            content_x,
            secondary_y,
            if compact_actions { 92.0 } else { 104.0 },
            28.0,
        ),
        handler,
        sel!(editContainerSession:),
        mtm,
    );
    let more = add_popup(
        parent,
        rect(
            content_x + if compact_actions { 102.0 } else { 114.0 },
            secondary_y,
            if compact_actions { 112.0 } else { 142.0 },
            28.0,
        ),
        &[
            "More…",
            "Duplicate Profile",
            "Export Profile",
            "View Raw Configuration",
            "Delete Profile",
        ],
        0,
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*edit, setTag: index as isize];
        let _: () = msg_send![&*edit, setToolTip:
            &*NSString::from_str("Edit this saved application profile. Running instances are not changed until their next launch.")];
        let _: () = msg_send![&*more, setTarget: handler];
        let _: () = msg_send![&*more, setAction:
            sel!(applicationProfileMoreAction:)];
        let _: () = msg_send![&*more, setTag: index as isize];
        let _: () = msg_send![&*more, setToolTip:
        &*NSString::from_str(if stop_enabled || launch_busy {
            "Duplicate, export, or inspect this profile. Stop the running instance before deleting it."
        } else {
            "Duplicate, export, inspect, or delete this saved application profile"
        })];
    }

    let presentation_y = if compact_actions {
        secondary_y - 38.0
    } else {
        secondary_y
    };
    let presentation = add_popup(
        parent,
        rect(
            content_x + if compact_actions { 0.0 } else { 270.0 },
            presentation_y,
            170.0,
            28.0,
        ),
        &["Desktop compositor", "Rootless Wayland apps"],
        usize::from(session.presentation_mode().is_rootless()),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*presentation, setTarget: handler];
        let _: () = msg_send![&*presentation, setAction:
            sel!(changeContainerSessionPresentation:)];
        let _: () = msg_send![&*presentation, setTag: index as isize];
        let _: () = msg_send![&*presentation, setToolTip:
            &*NSString::from_str("Use desktop for a compositor such as niri/Hyprland, or rootless for native Wayland applications such as foot")];
        if active.is_some() {
            let _: () = msg_send![&*presentation, setEnabled: false];
        }
    }

    let panel_top = if compact_actions {
        presentation_y - 24.0
    } else {
        secondary_y - 24.0
    };
    match selected_tab {
        1 => add_session_logs(
            parent,
            index,
            session,
            state,
            content_x,
            panel_top - 292.0,
            width - 84.0,
            mtm,
        ),
        2 => add_session_terminal(
            parent,
            index,
            session,
            content_x,
            panel_top - 226.0,
            width - 84.0,
            handler,
            mtm,
        ),
        3 => add_session_files(
            parent,
            session,
            content_x,
            panel_top - 178.0,
            width - 84.0,
            mtm,
        ),
        _ => add_session_info(
            parent,
            index,
            session,
            state,
            content_x,
            panel_top,
            width - 84.0,
            handler,
            mtm,
        ),
    }
}

fn add_session_info(
    parent: &NSView,
    index: usize,
    session: &ContainerSession,
    state: Option<&SessionState>,
    x: f64,
    top: f64,
    width: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let derived_transport_blocked = session_has_apple_transport_block(session);
    let active = active_session(index);
    let profile_status = state
        .map(|state| state.profile.label().to_string())
        .unwrap_or_else(|| "Checking".into());
    let instance_status = active
        .as_ref()
        .map(|active| active.instance.status.label().to_string())
        .or_else(|| {
            state
                .and_then(|state| state.instance)
                .map(|status| status.label().to_string())
        })
        .unwrap_or_else(|| "Not running".into());
    let overview_rows = vec![
        ("Profile status".to_string(), profile_status),
        ("Instance status".to_string(), instance_status),
        (
            "Runtime".to_string(),
            runtime_label(&session.runtime).to_string(),
        ),
        (
            "Presentation".to_string(),
            session_presentation_summary(session).to_string(),
        ),
        ("Image".to_string(), session.image.clone()),
        ("Command".to_string(), session_display_command(session)),
        (
            "Display policy".to_string(),
            session_display_summary(session),
        ),
    ];
    let mut cursor =
        add_labeled_rows_card(parent, "Overview", &overview_rows, x, top, width, mtm) - 18.0;

    let configuration_rows = vec![
        ("Waypipe".to_string(), session_waypipe_summary(session)),
        (
            "CPU".to_string(),
            runtime_arg_value(&session.runtime_args, "--cpus")
                .unwrap_or_else(|| "Runtime default".into()),
        ),
        (
            "Memory".to_string(),
            runtime_arg_value(&session.runtime_args, "--memory")
                .unwrap_or_else(|| "Runtime default".into()),
        ),
        (
            "Shared memory".to_string(),
            runtime_arg_value(&session.runtime_args, "--shm-size")
                .unwrap_or_else(|| "Runtime default".into()),
        ),
        (
            "Audio".to_string(),
            if session.audio {
                "48 kHz stereo playback".into()
            } else {
                "Off".into()
            },
        ),
        (
            "Mounts".to_string(),
            format_count(session.mounts.len(), "mount"),
        ),
        (
            "Environment".to_string(),
            format_count(session.env.len(), "variable"),
        ),
    ];
    cursor = add_labeled_rows_card(
        parent,
        "Configuration",
        &configuration_rows,
        x,
        cursor,
        width,
        mtm,
    ) - 18.0;

    if !session.runtime_args.is_empty() {
        cursor =
            add_runtime_arguments_card(parent, &session.runtime_args, x, cursor, width, mtm) - 18.0;
    }

    let instance_rows = if let Some(active) = active.as_ref() {
        vec![
            ("Instance".to_string(), format!("#{}", active.instance.id)),
            (
                "Status".to_string(),
                active.instance.status.label().to_string(),
            ),
            (
                "Started".to_string(),
                elapsed_time_label(active.instance.started_at_unix_ms),
            ),
            (
                "Container".to_string(),
                container_sessions::container_name(session),
            ),
            ("Display".to_string(), active.instance.display_slot.clone()),
        ]
    } else {
        vec![
            ("Status".to_string(), "No running instances".into()),
            (
                "Next launch".to_string(),
                format!(
                    "Uses the {} display policy",
                    session_display_target(session)
                ),
            ),
        ]
    };
    cursor = add_labeled_rows_card(
        parent,
        "Running Instances",
        &instance_rows,
        x,
        cursor,
        width,
        mtm,
    ) - 18.0;

    let status_detail = active
        .as_ref()
        .map(|_| "Application process and Waypipe worker are tracked by Cocoa-Way.".to_string())
        .or_else(|| state.map(|state| state.detail.clone()))
        .unwrap_or_else(|| {
            if derived_transport_blocked {
                apple_transport_blocked_detail(session)
            } else {
                "Run Health Check to validate this application before launch.".into()
            }
        });
    let mut diagnostic_rows = vec![
        (
            "Health".to_string(),
            state
                .map(|state| session_state_label(state).to_string())
                .unwrap_or_else(|| "Not checked".into()),
        ),
        ("Detail".to_string(), status_detail),
    ];
    if let Some(step) = state.and_then(|state| state.failed_step) {
        diagnostic_rows.push(("Failed step".to_string(), step.label().into()));
    }
    if let Some(active) = active.as_ref() {
        diagnostic_rows.push((
            "Processes".to_string(),
            format!(
                "container {}; waypipe {}",
                active
                    .instance
                    .container_pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "managed by runtime".into()),
                active.instance.waypipe_pid
            ),
        ));
    }
    let diagnostics_bottom = add_labeled_rows_card(
        parent,
        "Diagnostics",
        &diagnostic_rows,
        x,
        cursor,
        width,
        mtm,
    );
    let actions_height = 52.0;
    let actions_y = diagnostics_bottom - actions_height;
    add_detail_card(parent, x, actions_y, width, actions_height, mtm);
    let diagnostics_buttons_y = actions_y + 12.0;
    let logs = add_button(
        parent,
        "View Logs",
        rect(x + width - 242.0, diagnostics_buttons_y, 96.0, 28.0),
        handler,
        sel!(selectContainerTab:),
        mtm,
    );
    let raw = add_button(
        parent,
        "Raw Config",
        rect(x + width - 136.0, diagnostics_buttons_y, 112.0, 28.0),
        handler,
        sel!(viewRawContainerSession:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*logs, setTag: 1isize];
        let _: () = msg_send![&*raw, setTag: index as isize];
    }

    let launch_task = latest_operation_task(&launch_task_key(index))
        .filter(|task| task.status != TaskStatus::Completed);
    let mut supporting_card_y = actions_y - 116.0;
    if let Some(task) = launch_task.as_ref() {
        let task_height = 86.0
            + task.steps.len() as f64 * 22.0
            + if task.status == TaskStatus::Failed {
                38.0
            } else {
                0.0
            };
        let task_y = actions_y - 18.0 - task_height;
        add_application_task_card(parent, task, x, task_y, width, handler, mtm);
        supporting_card_y = task_y - 114.0;
    } else if state.is_some_and(|state| {
        state.profile == ProfileStatus::Invalid || state.instance == Some(InstanceStatus::Failed)
    }) {
        let error_y = actions_y - 130.0;
        add_application_error_card(
            parent,
            index,
            state.unwrap(),
            x,
            error_y,
            width,
            handler,
            mtm,
        );
        supporting_card_y = error_y - 114.0;
    }

    if state.is_some_and(is_missing_image_state) {
        add_missing_image_card(
            parent,
            index,
            session,
            x,
            supporting_card_y,
            width,
            handler,
            mtm,
        );
    } else if state.is_some_and(is_apple_container_stopped_state) {
        add_apple_container_stopped_card(parent, x, supporting_card_y, width, handler, mtm);
    } else if derived_transport_blocked {
        add_apple_container_transport_card(parent, x, supporting_card_y - 14.0, width, mtm);
    } else {
        add_display_note_card(parent, index, session, x, supporting_card_y, width, mtm);
    }
}

fn add_labeled_rows_card(
    parent: &NSView,
    title: &str,
    rows: &[(String, String)],
    x: f64,
    top: f64,
    width: f64,
    mtm: MainThreadMarker,
) -> f64 {
    let value_width = (width - 44.0 - 124.0).max(80.0);
    let row_heights = rows
        .iter()
        .map(|(_, value)| session_detail_row_height(value, value_width))
        .collect::<Vec<_>>();
    let card_height = 58.0 + row_heights.iter().sum::<f64>();
    let card_y = top - card_height;
    add_detail_card(parent, x, card_y, width, card_height, mtm);
    add_label(
        parent,
        title,
        rect(x + 22.0, top - 38.0, width - 44.0, 22.0),
        mtm,
        TextStyle::Heading,
    );
    let mut row_y = top - 68.0;
    for ((key, value), row_height) in rows.iter().zip(row_heights) {
        add_session_detail_row(parent, key, value, x + 22.0, row_y, width - 44.0, mtm);
        row_y -= row_height;
    }
    card_y
}

fn add_runtime_arguments_card(
    parent: &NSView,
    arguments: &[String],
    x: f64,
    top: f64,
    width: f64,
    mtm: MainThreadMarker,
) -> f64 {
    let text = format_runtime_arguments(arguments);
    let lines = text.lines().count().max(1).min(10);
    let text_height = lines as f64 * line_height_for_style(TextStyle::Mono);
    let height = 62.0 + text_height;
    let y = top - height;
    add_detail_card(parent, x, y, width, height, mtm);
    add_label(
        parent,
        "Runtime Arguments",
        rect(x + 22.0, top - 38.0, width - 44.0, 22.0),
        mtm,
        TextStyle::Heading,
    );
    let label = add_label(
        parent,
        &text,
        rect(x + 22.0, y + 16.0, width - 44.0, text_height),
        mtm,
        TextStyle::Mono,
    );
    unsafe {
        label.setSelectable(true);
        let _: () = msg_send![&*label, setToolTip: &*NSString::from_str(&text)];
    }
    y
}

fn runtime_arg_value(arguments: &[String], flag: &str) -> Option<String> {
    for (index, argument) in arguments.iter().enumerate() {
        if argument == flag {
            return arguments.get(index + 1).cloned();
        }
        if let Some(value) = argument.strip_prefix(&format!("{}=", flag)) {
            return Some(value.to_string());
        }
    }
    None
}

fn format_runtime_arguments(arguments: &[String]) -> String {
    let mut lines = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument.starts_with("--")
            && !argument.contains('=')
            && arguments
                .get(index + 1)
                .is_some_and(|value| !value.starts_with("--"))
        {
            lines.push(format!("{} {}", argument, arguments[index + 1]));
            index += 2;
        } else {
            lines.push(argument.clone());
            index += 1;
        }
    }
    lines.join("\n")
}

fn format_count(count: usize, noun: &str) -> String {
    format!("{} {}{}", count, noun, if count == 1 { "" } else { "s" })
}

fn elapsed_time_label(started_at_unix_ms: u128) -> String {
    let elapsed_ms = now_unix_ms().saturating_sub(started_at_unix_ms);
    let seconds = elapsed_ms / 1_000;
    if seconds < 60 {
        format!("{}s ago", seconds)
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

fn add_application_task_card(
    parent: &NSView,
    task: &OperationTask,
    x: f64,
    y: f64,
    width: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let failed = task.status == TaskStatus::Failed;
    let height = 86.0 + task.steps.len() as f64 * 22.0 + if failed { 38.0 } else { 0.0 };
    add_detail_card(parent, x, y, width, height, mtm);
    add_label(
        parent,
        &task.operation,
        rect(x + 22.0, y + height - 36.0, width - 148.0, 22.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        task.status.label(),
        rect(x + width - 118.0, y + height - 34.0, 96.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let detail = task.detail.as_deref().unwrap_or("Operation in progress");
    let detail_label = add_label(
        parent,
        &short_text(detail, chars_for_width(width - 44.0, TextStyle::Caption)),
        rect(x + 22.0, y + height - 60.0, width - 44.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    unsafe {
        let _: () = msg_send![&*detail_label, setToolTip: &*NSString::from_str(detail)];
    }

    let mut row_y = y + height - 84.0;
    for step in &task.steps {
        let status = match step.status {
            TaskStepStatus::Pending => "Pending",
            TaskStepStatus::Running => "In progress",
            TaskStepStatus::Completed => "Done",
            TaskStepStatus::Failed => "Failed",
        };
        add_label(
            parent,
            status,
            rect(x + 22.0, row_y, 88.0, 18.0),
            mtm,
            TextStyle::Caption,
        );
        add_label(
            parent,
            &step.name,
            rect(x + 116.0, row_y - 1.0, width - 138.0, 20.0),
            mtm,
            TextStyle::Body,
        );
        row_y -= 22.0;
    }

    if failed {
        let view_error = add_button(
            parent,
            "View Details",
            rect(x + 22.0, y + 10.0, 104.0, 28.0),
            handler,
            sel!(selectContainerTab:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*view_error, setTag: 1isize];
            let _: () = msg_send![&*view_error, setToolTip:
                &*NSString::from_str("Open captured launch logs for this failure")];
        }
    }
}

fn add_application_error_card(
    parent: &NSView,
    index: usize,
    state: &SessionState,
    x: f64,
    y: f64,
    width: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_detail_card(parent, x, y, width, 112.0, mtm);
    let title = state
        .failed_step
        .map(|step| format!("{} failed", step.label()))
        .unwrap_or_else(|| "Application validation failed".into());
    add_label(
        parent,
        &title,
        rect(x + 22.0, y + 78.0, width - 44.0, 22.0),
        mtm,
        TextStyle::Heading,
    );
    let detail = compact_detail(&state.detail);
    let detail_label = add_label(
        parent,
        &detail,
        rect(x + 22.0, y + 50.0, width - 44.0, 20.0),
        mtm,
        TextStyle::Caption,
    );
    unsafe {
        let _: () = msg_send![&*detail_label, setToolTip: &*NSString::from_str(&state.detail)];
    }
    let retry = add_button(
        parent,
        "Retry Check",
        rect(x + 22.0, y + 10.0, 104.0, 28.0),
        handler,
        sel!(checkContainerSession:),
        mtm,
    );
    let view_error = add_button(
        parent,
        "View Details",
        rect(x + 136.0, y + 10.0, 104.0, 28.0),
        handler,
        sel!(selectContainerTab:),
        mtm,
    );
    let copy_diagnostics = add_button(
        parent,
        "Copy Diagnostics",
        rect(
            x + 250.0,
            y + 10.0,
            (width - 272.0).clamp(92.0, 124.0),
            28.0,
        ),
        handler,
        sel!(copyApplicationDiagnostics:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*retry, setTag: index as isize];
        let _: () = msg_send![&*view_error, setTag: 1isize];
        let _: () = msg_send![&*copy_diagnostics, setTag: index as isize];
        let _: () = msg_send![&*copy_diagnostics, setToolTip:
            &*NSString::from_str("Copy profile, state, display, and recent logs")];
    }
}

fn session_detail_row_height(value: &str, value_width: f64) -> f64 {
    let line_chars = chars_for_width(value_width, TextStyle::Body);
    if value.chars().count() > line_chars {
        42.0
    } else {
        26.0
    }
}

fn add_session_detail_row(
    parent: &NSView,
    key: &str,
    value: &str,
    x: f64,
    y: f64,
    width: f64,
    mtm: MainThreadMarker,
) {
    add_label(
        parent,
        key,
        rect(x, y, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let value_width = (width - 124.0).max(80.0);
    let row_height = session_detail_row_height(value, value_width);
    let max_chars =
        chars_for_width(value_width, TextStyle::Body) * if row_height > 26.0 { 2 } else { 1 };
    let visible = short_text(value, max_chars);
    let label = add_label(
        parent,
        &visible,
        rect(
            x + 124.0,
            if row_height > 26.0 { y - 17.0 } else { y - 1.0 },
            value_width,
            if row_height > 26.0 { 36.0 } else { 20.0 },
        ),
        mtm,
        TextStyle::Body,
    );
    if visible != value {
        unsafe {
            let _: () = msg_send![&*label, setToolTip: &*NSString::from_str(value)];
        }
    }
}

fn is_missing_image_state(state: &SessionState) -> bool {
    state.detail.contains("not available locally")
}

fn is_apple_container_stopped_state(state: &SessionState) -> bool {
    state.detail.contains("container system start")
        || state.detail.contains("reports running")
        || state.detail.contains("not running")
}

fn is_local_image_reference(image: &str) -> bool {
    image.starts_with("localhost/") || image.starts_with("localhost:") || !image.contains('/')
}

fn is_smoke_image_reference(image: &str) -> bool {
    image.contains("cocoa-way-niri") || image.contains("cocoa-way-smoke")
}

fn smoke_image_build_command() -> String {
    format!(
        "container build -f {} -t {} {}",
        smoke_containerfile_path(),
        smoke_image_reference(),
        smoke_build_context()
    )
}

fn add_missing_image_card(
    parent: &NSView,
    index: usize,
    session: &ContainerSession,
    x: f64,
    y: f64,
    width: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let local_image = is_local_image_reference(&session.image);
    add_detail_card(parent, x, y, width, 96.0, mtm);
    add_label(
        parent,
        if local_image {
            "Local image missing"
        } else {
            "Missing image"
        },
        rect(x + 22.0, y + 60.0, width - 44.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    let message = if local_image {
        format!(
            "'{}' is a local tag. Build/export OCI, then load it.",
            short_text(&session.image, 48)
        )
    } else {
        format!(
            "'{}' is not in Apple Container yet.",
            short_text(&session.image, 54)
        )
    };
    add_label(
        parent,
        &message,
        rect(x + 22.0, y + 36.0, width - 44.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let mut button_x = x + 22.0;
    if local_image && is_smoke_image_reference(&session.image) {
        let build = add_button(
            parent,
            "Build",
            rect(button_x, y + 6.0, 78.0, 28.0),
            handler,
            sel!(buildSmokeContainerSessionImage:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*build, setTag: index as isize];
            let _: () = msg_send![&*build, setToolTip:
                &*NSString::from_str("Build the bundled example image with Apple Container")];
        }
        button_x += 88.0;
    } else if !local_image {
        let pull = add_button(
            parent,
            "Pull Image",
            rect(button_x, y + 6.0, 96.0, 28.0),
            handler,
            sel!(pullContainerSessionImage:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*pull, setTag: index as isize];
            let _: () = msg_send![&*pull, setToolTip:
                &*NSString::from_str("Pull the application image with Apple Container")];
        }
        button_x += 106.0;
    }
    let load = add_button(
        parent,
        "Load OCI",
        rect(button_x, y + 6.0, 88.0, 28.0),
        handler,
        sel!(loadContainerSessionImage:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*load, setTag: index as isize];
        let _: () = msg_send![&*load, setToolTip:
            &*NSString::from_str("Load an OCI archive into Apple Container")];
    }
    if local_image && is_smoke_image_reference(&session.image) {
        let build = add_button(
            parent,
            "Copy Build Cmd",
            rect(button_x + 98.0, y + 6.0, 128.0, 28.0),
            handler,
            sel!(copySmokeImageBuildCommand:),
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*build, setToolTip:
                &*NSString::from_str("Copy an Apple Container build command for the bundled example image")];
        }
    }
}

fn add_apple_container_stopped_card(
    parent: &NSView,
    x: f64,
    y: f64,
    width: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_detail_card(parent, x, y, width, 96.0, mtm);
    add_label(
        parent,
        "Apple Container is stopped",
        rect(x + 22.0, y + 60.0, width - 44.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Start the Apple Container system, then run Check again.",
        rect(x + 22.0, y + 36.0, width - 44.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    let start = add_button(
        parent,
        "Start System",
        rect(x + 22.0, y + 6.0, 112.0, 28.0),
        handler,
        sel!(startAppleContainerSystem:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*start, setToolTip:
            &*NSString::from_str("Run `container system start`")];
    }
}

fn add_display_note_card(
    parent: &NSView,
    index: usize,
    session: &ContainerSession,
    x: f64,
    y: f64,
    width: f64,
    mtm: MainThreadMarker,
) {
    let requested = session_display_target(session);
    let active = active_session(index);
    let display_slot = active
        .as_ref()
        .map(|active| active.instance.display_slot.clone());
    let telemetry = active_session_performance(index);
    let (title, detail, behavior) = if session.presentation_mode().is_rootless() {
        (
            "Rootless Wayland apps".to_string(),
            "Each native Wayland app toplevel is mapped to its own macOS window.".to_string(),
            if active.is_some() {
                "An isolated compositor worker owns this application until it is stopped."
                    .to_string()
            } else {
                "Launch creates an isolated worker without occupying the desktop display."
                    .to_string()
            },
        )
    } else if let Some(active) = active.as_ref() {
        if active.instance.display_slot == "default" {
            (
                "Default display".to_string(),
                "This session is using the current Cocoa-Way display window.".to_string(),
                "Stop it to release the default display for another auto session.".to_string(),
            )
        } else {
            (
                format!("Dedicated display: {}", active.instance.display_slot),
                "This session owns an independent Metal window and Wayland socket.".to_string(),
                "Stopping the session also closes and cleans up its display worker.".to_string(),
            )
        }
    } else if requested == "auto" {
        (
            "Automatic display".to_string(),
            "Auto uses the default Cocoa-Way window when it is available.".to_string(),
            "If it is occupied, Cocoa-Way creates a dedicated display automatically.".to_string(),
        )
    } else if requested == "default" {
        (
            "Default display".to_string(),
            "This profile always targets the current Cocoa-Way display window.".to_string(),
            "Launch is blocked while another session owns the default display.".to_string(),
        )
    } else {
        (
            format!("Dedicated display: {}", requested),
            "This profile launches in an independent Cocoa-Way display window.".to_string(),
            "The named display is recreated on launch and cleaned up on stop.".to_string(),
        )
    };
    add_detail_card(parent, x, y, width, 96.0, mtm);
    let title_label = add_label(
        parent,
        &display_fps_text(&title, telemetry.as_ref()),
        rect(x + 22.0, y + 60.0, width - 44.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    if let Some(display_slot) = display_slot {
        register_live_display_fps_label(&title_label, display_slot, title);
    }
    add_label(
        parent,
        &detail,
        rect(x + 22.0, y + 36.0, width - 44.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        &behavior,
        rect(x + 22.0, y + 18.0, width - 44.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
}

fn compact_detail(value: &str) -> String {
    const MAX: usize = 120;
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX {
        normalized
    } else {
        let mut truncated = normalized.chars().take(MAX - 3).collect::<String>();
        truncated.push_str("...");
        truncated
    }
}

fn add_session_logs(
    parent: &NSView,
    index: usize,
    session: &ContainerSession,
    state: Option<&SessionState>,
    x: f64,
    y: f64,
    width: f64,
    mtm: MainThreadMarker,
) {
    add_detail_card(parent, x, y, width, 292.0, mtm);
    add_label(
        parent,
        "Launch Logs",
        rect(x + 22.0, y + 244.0, width - 44.0, 24.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Live stdout/stderr from the container runtime and waypipe client is captured for this app session.",
        rect(x + 22.0, y + 212.0, width - 44.0, 30.0),
        mtm,
        TextStyle::Body,
    );
    let logs = session_logs(index);
    if logs.is_empty() {
        let fallback = state
            .map(|state| state.detail.clone())
            .unwrap_or_else(|| format!("No process output captured for '{}'.", session.name));
        add_label(
            parent,
            &fallback,
            rect(x + 22.0, y + 170.0, width - 44.0, 26.0),
            mtm,
            TextStyle::Caption,
        );
        return;
    }

    let max_chars = ((width - 44.0) / 10.0).floor().clamp(48.0, 120.0) as usize;
    let visible_lines = wrapped_log_lines(&logs, max_chars, 8);
    let mut row_y = y + 176.0;
    for line in visible_lines {
        add_label(
            parent,
            &line,
            rect(x + 22.0, row_y, width - 44.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        row_y -= 22.0;
    }
}

fn add_session_terminal(
    parent: &NSView,
    index: usize,
    session: &ContainerSession,
    x: f64,
    y: f64,
    width: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_detail_card(parent, x, y, width, 226.0, mtm);
    add_label(
        parent,
        "Terminal Bridge",
        rect(x + 22.0, y + 178.0, width - 44.0, 24.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Open a macOS Terminal shell inside the running GUI container. Launch the session first, then attach here.",
        rect(x + 22.0, y + 142.0, width - 44.0, 34.0),
        mtm,
        TextStyle::Body,
    );
    add_label(
        parent,
        &format!(
            "Target runtime: {}    container: {}",
            runtime_label(&session.runtime),
            container_sessions::container_name(session)
        ),
        rect(x + 22.0, y + 104.0, width - 44.0, 24.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        &container_sessions::terminal_command(session),
        rect(x + 22.0, y + 70.0, width - 44.0, 20.0),
        mtm,
        TextStyle::Mono,
    );
    let button = add_button(
        parent,
        "Open Shell",
        rect(x + 22.0, y + 24.0, 116.0, 30.0),
        handler,
        sel!(openContainerTerminal:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*button, setTag: index as isize];
        let _: () = msg_send![&*button, setToolTip:
            &*NSString::from_str("Open macOS Terminal and attach to this running GUI container")];
        if active_session(index).is_none() {
            let _: () = msg_send![&*button, setEnabled: false];
            let _: () = msg_send![&*button, setToolTip:
                &*NSString::from_str("Launch this application before opening a shell")];
        }
    }
}

fn add_session_files(
    parent: &NSView,
    session: &ContainerSession,
    x: f64,
    y: f64,
    width: f64,
    mtm: MainThreadMarker,
) {
    add_detail_card(parent, x, y, width, 178.0, mtm);
    add_label(
        parent,
        "Shared Files",
        rect(x + 22.0, y + 130.0, width - 44.0, 24.0),
        mtm,
        TextStyle::Heading,
    );
    if session.mounts.is_empty() {
        add_label(
            parent,
            "Declare mounts in container-sessions.toml to share project folders with this application.",
            rect(x + 22.0, y + 94.0, width - 44.0, 34.0),
            mtm,
            TextStyle::Body,
        );
        add_label(
            parent,
            &format!("No file mounts are declared for '{}'.", session.name),
            rect(x + 22.0, y + 54.0, width - 44.0, 26.0),
            mtm,
            TextStyle::Caption,
        );
    } else {
        add_label(
            parent,
            "Mounted folders are passed to the container runtime at launch.",
            rect(x + 22.0, y + 98.0, width - 44.0, 28.0),
            mtm,
            TextStyle::Body,
        );
        let mut row_y = y + 64.0;
        for mount in session.mounts.iter().take(3) {
            add_label(
                parent,
                mount,
                rect(x + 22.0, row_y, width - 44.0, 18.0),
                mtm,
                TextStyle::Mono,
            );
            row_y -= 22.0;
        }
    }
}

fn add_apple_container_transport_card(
    parent: &NSView,
    x: f64,
    y: f64,
    width: f64,
    mtm: MainThreadMarker,
) {
    add_detail_card(parent, x, y, width, 116.0, mtm);
    add_label(
        parent,
        "GUI relay unavailable",
        rect(x + 22.0, y + 78.0, width - 44.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    add_label(
        parent,
        "Apple Container GUI launch needs Transport V2 or the stdio compatibility relay.",
        rect(x + 22.0, y + 42.0, width - 44.0, 34.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        "Run Check, then inspect Logs if Launch cannot start the relay.",
        rect(x + 22.0, y + 18.0, width - 44.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
}

fn add_image_detail(
    parent: &NSView,
    image: &SelectedImage,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let content_x = x + 42.0;
    let header_y = y + height - 124.0;
    add_label(
        parent,
        &short_text(&image.reference, 52),
        rect(content_x, header_y, width - 84.0, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        &format!("{} image in the local runtime store", image.runtime),
        rect(content_x, header_y - 28.0, width - 84.0, 20.0),
        mtm,
        TextStyle::Body,
    );

    let create_index = {
        let mut actions = IMAGE_CREATE_ACTIONS.lock().unwrap();
        let action_index = actions.len();
        actions.push((image.runtime_key.clone(), image.reference.clone()));
        action_index
    };
    let delete_index = {
        let mut actions = IMAGE_DELETE_ACTIONS.lock().unwrap();
        let action_index = actions.len();
        actions.push(ImageDeleteAction {
            runtime: image.runtime_key.clone(),
            reference: image.reference.clone(),
            image_id: image_id_from_label(&image.label, &image.reference),
        });
        action_index
    };
    let create = add_button(
        parent,
        "Create Application",
        rect(content_x, header_y - 72.0, 142.0, 30.0),
        handler,
        sel!(createContainerSessionFromImage:),
        mtm,
    );
    let image_has_tag = image_reference_has_tag(&image.reference);
    let more = add_popup(
        parent,
        rect(content_x + 154.0, header_y - 72.0, 126.0, 30.0),
        if image_has_tag {
            &["More…", "Remove Tag", "Delete Image"]
        } else {
            &["More…", "Delete Image"]
        },
        0,
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*create, setTag: create_index as isize];
        let _: () = msg_send![&*create, setToolTip:
            &*NSString::from_str("Create an application profile from this image")];
        let _: () = msg_send![&*more, setTarget: handler];
        let _: () = msg_send![&*more, setAction: sel!(imageMoreAction:)];
        let _: () = msg_send![&*more, setTag: delete_index as isize];
        let _: () = msg_send![&*more, setToolTip:
            &*NSString::from_str("Remove a tag or delete the underlying image after dependency checks")];
    }

    let (repository, tag) = split_image_reference(&image.reference);
    let image_id = image_id_from_label(&image.label, &image.reference);
    let known_tags = image_id
        .as_deref()
        .map(|image_id| image_references_for_id(&image.runtime_key, image_id))
        .filter(|references| !references.is_empty())
        .unwrap_or_else(|| vec![image.reference.clone()]);
    let sessions = container_sessions::load_sessions();
    let referenced_profiles = sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| {
            known_tags.contains(&session.image)
                && runtime_key_matches(&image.runtime_key, &session.runtime)
        })
        .map(|(index, session)| (index, session.name.clone()))
        .collect::<Vec<_>>();
    let running_instances = referenced_profiles
        .iter()
        .filter(|(index, _)| active_session(*index).is_some())
        .map(|(_, name)| name.clone())
        .collect::<Vec<_>>();
    let rows = vec![
        ("Repository".into(), repository.into()),
        ("Tag".into(), tag.into()),
        (
            "Image ID".into(),
            image_id.unwrap_or_else(|| "Unknown".into()),
        ),
        ("Runtime".into(), image.runtime.clone()),
        (
            "Known Tags".into(),
            if known_tags.is_empty() {
                "None".into()
            } else {
                known_tags.join("\n")
            },
        ),
        (
            "Applications".into(),
            if referenced_profiles.is_empty() {
                "None".into()
            } else {
                referenced_profiles
                    .iter()
                    .map(|(_, name)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
        (
            "Running".into(),
            if running_instances.is_empty() {
                "0 instances".into()
            } else {
                format!(
                    "{}: {}",
                    format_count(running_instances.len(), "instance"),
                    running_instances.join(", ")
                )
            },
        ),
    ];
    let card_y = add_labeled_rows_card(
        parent,
        "Image Details",
        &rows,
        content_x,
        header_y - 112.0,
        width - 84.0,
        mtm,
    );

    let inspect_y = card_y - 178.0;
    add_detail_card(parent, content_x, inspect_y, width - 84.0, 158.0, mtm);
    add_label(
        parent,
        "Inspect Preview",
        rect(content_x + 22.0, inspect_y + 118.0, width - 128.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    let lines = resource_preview_lines(&image.runtime_key, "image", "inspect", &image.reference);
    let mut line_y = inspect_y + 90.0;
    for line in lines.iter().take(4) {
        add_label(
            parent,
            line,
            rect(content_x + 22.0, line_y, width - 128.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        line_y -= 22.0;
    }
}

fn add_volume_detail(
    parent: &NSView,
    volume: &SelectedVolume,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let content_x = x + 42.0;
    let header_y = y + height - 124.0;
    add_label(
        parent,
        &short_text(&volume.name, 52),
        rect(content_x, header_y, width - 84.0, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        &format!("{} local volume", volume.runtime),
        rect(content_x, header_y - 28.0, width - 84.0, 20.0),
        mtm,
        TextStyle::Body,
    );

    let delete_index = {
        let mut actions = VOLUME_DELETE_ACTIONS.lock().unwrap();
        let action_index = actions.len();
        actions.push(VolumeDeleteAction {
            runtime: volume.runtime_key.clone(),
            name: volume.name.clone(),
        });
        action_index
    };
    let usage = volume_usage(&volume.runtime_key, &volume.name);
    let metadata = volume_inspect_metadata(&volume.runtime_key, &volume.name, &volume.label);
    let delete = add_button(
        parent,
        "Delete Volume",
        rect(content_x, header_y - 72.0, 124.0, 30.0),
        handler,
        sel!(deleteLocalContainerVolume:),
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*delete, setTag: delete_index as isize];
        let blocked =
            usage.loading || usage.error.is_some() || !usage.mounted_containers.is_empty();
        let _: () = msg_send![&*delete, setEnabled: !blocked];
        let tooltip = if !usage.mounted_containers.is_empty() {
            format!("Mounted by: {}", usage.mounted_containers.join(", "))
        } else if usage.loading {
            "Wait for the volume usage check to finish".into()
        } else if let Some(error) = usage.error.as_deref() {
            format!("Usage could not be verified: {error}")
        } else {
            "Delete this volume after confirmation".into()
        };
        let _: () = msg_send![&*delete, setToolTip: &*NSString::from_str(&tooltip)];
    }

    let rows = vec![
        ("Name".into(), volume.name.clone()),
        ("Runtime".into(), volume.runtime.clone()),
        ("Type / Driver".into(), metadata.kind),
        ("Size".into(), metadata.size),
        ("Created".into(), metadata.created),
        (
            "Referenced Profiles".into(),
            if usage.referenced_profiles.is_empty() {
                "None".into()
            } else {
                usage.referenced_profiles.join(", ")
            },
        ),
        (
            "Mounted Containers".into(),
            if usage.loading {
                "Checking...".into()
            } else if let Some(error) = usage.error.as_deref() {
                format!("Unavailable: {error}")
            } else if usage.mounted_containers.is_empty() {
                "None".into()
            } else {
                usage.mounted_containers.join(", ")
            },
        ),
    ];
    let card_y = add_labeled_rows_card(
        parent,
        "Volume Details",
        &rows,
        content_x,
        header_y - 112.0,
        width - 84.0,
        mtm,
    );

    let inspect_y = card_y - 178.0;
    add_detail_card(parent, content_x, inspect_y, width - 84.0, 158.0, mtm);
    add_label(
        parent,
        "Inspect Preview",
        rect(content_x + 22.0, inspect_y + 118.0, width - 128.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    let lines = resource_preview_lines(&volume.runtime_key, "volume", "inspect", &volume.name);
    let mut line_y = inspect_y + 90.0;
    for line in lines.iter().take(4) {
        add_label(
            parent,
            line,
            rect(content_x + 22.0, line_y, width - 128.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        line_y -= 22.0;
    }
}

fn add_runtime_container_detail(
    parent: &NSView,
    container: &SelectedRuntimeContainer,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let content_x = x + 42.0;
    let content_width = width - 84.0;
    let header_y = y + height - 124.0;
    add_label(
        parent,
        &short_text(
            &container.name,
            chars_for_width(content_width, TextStyle::Title),
        ),
        rect(content_x, header_y, content_width, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        &format!(
            "{} container · {}",
            runtime_label(&container.runtime),
            if container.running {
                "Running"
            } else {
                "Stopped"
            }
        ),
        rect(content_x, header_y - 28.0, content_width, 20.0),
        mtm,
        TextStyle::Body,
    );

    let action_index = push_runtime_container_action(&container.runtime, &container.name);
    let primary = add_button(
        parent,
        if container.running { "Stop" } else { "Start" },
        rect(content_x, header_y - 72.0, 88.0, 30.0),
        handler,
        if container.running {
            sel!(stopRuntimeContainer:)
        } else {
            sel!(startRuntimeContainer:)
        },
        mtm,
    );
    let restart = add_button(
        parent,
        "Restart",
        rect(content_x + 98.0, header_y - 72.0, 88.0, 30.0),
        handler,
        sel!(restartRuntimeContainer:),
        mtm,
    );
    let terminal = add_button(
        parent,
        "Terminal",
        rect(content_x + 196.0, header_y - 72.0, 96.0, 30.0),
        handler,
        sel!(openRuntimeContainerTerminal:),
        mtm,
    );
    let refresh = add_button(
        parent,
        "Refresh",
        rect(content_x, header_y - 110.0, 88.0, 30.0),
        handler,
        sel!(refreshRuntimeContainerDetails:),
        mtm,
    );
    let delete = add_button(
        parent,
        "Delete",
        rect(content_x + 98.0, header_y - 110.0, 88.0, 30.0),
        handler,
        sel!(deleteRuntimeContainer:),
        mtm,
    );
    unsafe {
        for button in [&primary, &restart, &terminal, &delete] {
            let _: () = msg_send![&**button, setTag: action_index as isize];
        }
        let _: () = msg_send![&*primary, setToolTip:
            &*NSString::from_str(if container.running { "Stop this container" } else { "Start this container" })];
        let _: () = msg_send![&*restart, setToolTip:
            &*NSString::from_str("Restart this container and refresh its details")];
        let _: () = msg_send![&*terminal, setToolTip:
            &*NSString::from_str("Open an interactive shell in macOS Terminal")];
        let _: () = msg_send![&*refresh, setToolTip:
            &*NSString::from_str("Reload inspect, resource, and recent log output")];
        let _: () = msg_send![&*delete, setToolTip:
            &*NSString::from_str("Delete this container after confirmation")];
        let _: () = msg_send![&*restart, setEnabled: container.running];
        let _: () = msg_send![&*terminal, setEnabled: container.running];
    }

    let details = RUNTIME_CONTAINER_DETAILS
        .lock()
        .unwrap()
        .clone()
        .filter(|details| details.runtime == container.runtime && details.name == container.name);
    let mut info = details
        .as_ref()
        .map(|details| details.info.clone())
        .unwrap_or_else(|| vec!["Loading runtime details...".into()]);
    if let Some(error) = details.as_ref().and_then(|details| details.error.as_ref()) {
        info.push(format!("Warning: {}", error));
    }
    if info.is_empty() {
        info.push(container.label.clone());
    }
    let stats = details
        .as_ref()
        .map(|details| details.stats.clone())
        .unwrap_or_else(|| vec!["Waiting for a one-shot resource sample...".into()]);
    let logs = details
        .as_ref()
        .map(|details| details.logs.clone())
        .unwrap_or_else(|| vec!["Waiting for recent container logs...".into()]);

    let info_y = header_y - 286.0;
    add_runtime_output_card(
        parent,
        "Info",
        &info,
        content_x,
        info_y,
        content_width,
        140.0,
        4,
        mtm,
    );
    let stats_y = info_y - 136.0;
    add_runtime_output_card(
        parent,
        "Resources",
        &stats,
        content_x,
        stats_y,
        content_width,
        112.0,
        3,
        mtm,
    );
    let logs_y = stats_y - 250.0;
    add_runtime_output_card(
        parent,
        "Recent Logs",
        &logs,
        content_x,
        logs_y,
        content_width,
        226.0,
        8,
        mtm,
    );
}

#[allow(clippy::too_many_arguments)]
fn add_runtime_output_card(
    parent: &NSView,
    title: &str,
    lines: &[String],
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    max_rows: usize,
    mtm: MainThreadMarker,
) {
    add_detail_card(parent, x, y, width, height, mtm);
    add_label(
        parent,
        title,
        rect(x + 22.0, y + height - 38.0, width - 44.0, 20.0),
        mtm,
        TextStyle::Heading,
    );
    let max_chars = chars_for_width(width - 44.0, TextStyle::Mono);
    let visible = wrapped_log_lines(lines, max_chars, max_rows);
    let mut line_y = y + height - 66.0;
    for line in visible {
        add_label(
            parent,
            &line,
            rect(x + 22.0, line_y, width - 44.0, 18.0),
            mtm,
            TextStyle::Mono,
        );
        line_y -= 22.0;
    }
}

fn add_section_detail(
    parent: &NSView,
    title: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    mtm: MainThreadMarker,
) {
    let detail = match title {
        "Images" => "Create sessions, pull/load images, and keep cleanup separate.",
        "Volumes" => "Inspect local volumes. Deletes ask for confirmation.",
        "Displays" => {
            "Create managed display windows for scripts and explicit session assignment, or let auto use the default window and allocate dedicated displays."
        }
        "Activity" => "Recent Container Mode actions and runtime output are shown on the left.",
        "Commands" => {
            "Copy launch helper commands from the left when you need to debug outside the GUI."
        }
        "Docker" => {
            "Use the left pane to inspect Docker-compatible containers and stop/delete visible entries."
        }
        "OrbStack" => {
            "Use the left pane to inspect OrbStack state and manage Docker-compatible containers."
        }
        _ => "Runtime status and diagnostics are shown on the left.",
    };
    let content_x = x + 42.0;
    let content_y = y + height * 0.52;
    add_label(
        parent,
        title,
        rect(content_x, content_y, width - 84.0, 42.0),
        mtm,
        TextStyle::Hero,
    );
    add_label(
        parent,
        detail,
        rect(content_x, content_y - 48.0, width - 84.0, 42.0),
        mtm,
        TextStyle::Body,
    );
}

fn add_detail_card(
    parent: &NSView,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(x, y, width, height), mtm);
}

fn add_tab_bar(
    parent: &NSView,
    x: f64,
    y: f64,
    width: f64,
    selected_tab: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    add_card(parent, rect(x, y, width, 36.0), mtm);
    let tab_w = width / 4.0;
    add_card(
        parent,
        rect(
            x + selected_tab as f64 * tab_w + 4.0,
            y + 4.0,
            tab_w - 8.0,
            28.0,
        ),
        mtm,
    );
    for (index, title) in ["Overview", "Logs", "Terminal", "Files"].iter().enumerate() {
        let label_width = tab_w - 16.0;
        add_label(
            parent,
            title,
            rect(x + index as f64 * tab_w + 8.0, y + 9.0, label_width, 18.0),
            mtm,
            if index == selected_tab {
                TextStyle::Heading
            } else {
                TextStyle::Body
            },
        );
        add_hit_button(
            parent,
            rect(x + index as f64 * tab_w, y, tab_w, 36.0),
            index,
            handler,
            sel!(selectContainerTab:),
            mtm,
        );
    }
}

fn add_runtime_summary(parent: &NSView, x: f64, y: f64, width: f64, mtm: MainThreadMarker) {
    let diagnostics = runtime_diagnostics(&[]);
    let columns = diagnostics.len().max(1);
    let item_w = width / columns as f64;
    for (i, diagnostic) in diagnostics.iter().enumerate() {
        let item_x = x + i as f64 * item_w;
        add_label(
            parent,
            diagnostic.name,
            rect(item_x, y + 30.0, item_w - 14.0, 16.0),
            mtm,
            TextStyle::Caption,
        );
        let state_label = add_label(
            parent,
            &short_text(
                &diagnostic.state,
                chars_for_width(item_w - 14.0, TextStyle::Heading),
            ),
            rect(item_x, y + 10.0, item_w - 14.0, 20.0),
            mtm,
            TextStyle::Heading,
        );
        if diagnostic.name == "Display FPS" {
            *SUMMARY_FPS_LABEL.lock().unwrap() = Some(Retained::as_ptr(&state_label) as usize);
        }
    }
}

fn nav_title(index: usize) -> &'static str {
    match index {
        NAV_RUNNING => "Running",
        NAV_IMAGES => "Images",
        NAV_VOLUMES => "Volumes",
        NAV_DISPLAYS => "Displays",
        NAV_APPLE_CONTAINER => "Apple Container",
        NAV_DOCKER => "Docker-compatible",
        NAV_ORBSTACK => "OrbStack",
        NAV_ACTIVITY => "Activity",
        NAV_COMMANDS => "Commands",
        _ => "Applications",
    }
}

fn detail_empty_message(index: usize) -> &'static str {
    match index {
        1 => "Select an application profile to inspect its launch logs and Waypipe output.",
        2 => "Select a running application instance to open an interactive terminal.",
        3 => "Select an application profile to inspect files exported from its container.",
        _ => "Select an application profile to inspect its configuration and running instance.",
    }
}

fn add_placeholder_list(
    parent: &NSView,
    width: f64,
    content_height: f64,
    title: &str,
    mtm: MainThreadMarker,
) {
    let center_y = (content_height * 0.54).max(250.0);
    add_label(
        parent,
        &format!("No {}", title),
        rect(34.0, center_y, width - 68.0, 34.0),
        mtm,
        TextStyle::Title,
    );
    add_label(
        parent,
        "No local resources are available for this section.",
        rect(34.0, center_y - 44.0, width - 68.0, 54.0),
        mtm,
        TextStyle::Body,
    );
}

struct RuntimeDiagnostic {
    name: &'static str,
    state: String,
}

fn runtime_diagnostics(_sessions: &[ContainerSession]) -> Vec<RuntimeDiagnostic> {
    let child_path = build_child_path();
    let fps = summary_performance_snapshot()
        .map(|snapshot| display_fps_state(&snapshot))
        .unwrap_or_else(|| "Waiting".into());
    let storage = crate::diagnostics::resource_snapshot()
        .disk_available_bytes
        .map(|bytes| format!("{:.1} GiB", crate::diagnostics::bytes_to_gib(bytes)))
        .unwrap_or_else(|| "Unknown".into());
    vec![
        apple_container_diagnostic(&child_path),
        apple_gui_transport_diagnostic(&child_path),
        RuntimeDiagnostic {
            name: "Display FPS",
            state: fps,
        },
        RuntimeDiagnostic {
            name: "Apple Free",
            state: storage,
        },
        RuntimeDiagnostic {
            name: "Tasks",
            state: format!("{} active", active_task_count()),
        },
    ]
}

fn apple_container_diagnostic(child_path: &str) -> RuntimeDiagnostic {
    let Some(path) = find_command_path("container", child_path) else {
        return RuntimeDiagnostic {
            name: "Apple Mgmt",
            state: "Missing".into(),
        };
    };

    let _ = path;
    let state = if crate::diagnostics::resource_snapshot().available {
        "Running"
    } else {
        "Installed"
    };

    RuntimeDiagnostic {
        name: "Apple Mgmt",
        state: state.into(),
    }
}

fn apple_gui_transport_diagnostic(child_path: &str) -> RuntimeDiagnostic {
    let Some(container) = find_command_path("container", child_path) else {
        return RuntimeDiagnostic {
            name: "GUI Transport",
            state: "Missing".into(),
        };
    };

    RuntimeDiagnostic {
        name: "GUI Transport",
        state: if container_sessions::apple_publish_socket_supported(&container, child_path) {
            "V2 Ready".into()
        } else {
            "Fallback".into()
        },
    }
}

fn apple_container_data_root() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".into());
    format!("{}/Library/Application Support/com.apple.container", home)
}

unsafe fn add_session_row(
    parent: &NSView,
    session: &ContainerSession,
    index: usize,
    selected: bool,
    state: Option<SessionState>,
    y: f64,
    width: f64,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let card_w = (width - 24.0).max(320.0);
    let process_active = active_session(index).is_some();
    let state_label = state
        .as_ref()
        .map(|state| session_state_label(&state))
        .unwrap_or(if process_active {
            "Running"
        } else if session_has_apple_transport_block(session) {
            "Blocked"
        } else {
            "Validating"
        });
    let missing_image = state.as_ref().is_some_and(is_missing_image_state);
    let transport_blocked = session_has_apple_transport_block(session);
    let launch_busy = process_active || session_is_launch_busy(state.as_ref());
    let card_frame = rect(12.0, y + 10.0, card_w, 160.0);
    add_profile_card(
        parent,
        card_frame,
        selected,
        runtime_nav(&session.runtime),
        mtm,
    );
    add_label(
        parent,
        &session.name,
        rect(32.0, y + 136.0, card_w - 174.0, 24.0),
        mtm,
        TextStyle::Heading,
    );
    let status = add_label(
        parent,
        state_label,
        rect(card_w - 132.0, y + 138.0, 112.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    if let Some(detail) = state.as_ref().map(|state| state.detail.as_str()) {
        unsafe {
            let _: () = msg_send![&*status, setToolTip: &*NSString::from_str(detail)];
        }
    }
    add_label(
        parent,
        &format!(
            "{} · {}",
            session_display_command(session),
            session_presentation_summary(session)
        ),
        rect(32.0, y + 112.0, card_w - 52.0, 20.0),
        mtm,
        TextStyle::Body,
    );
    add_label(
        parent,
        runtime_label(&session.runtime),
        rect(32.0, y + 90.0, card_w - 52.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    add_label(
        parent,
        &short_image_label(&session.image, 54),
        rect(32.0, y + 68.0, card_w - 52.0, 20.0),
        mtm,
        TextStyle::Body,
    );
    add_label(
        parent,
        if process_active {
            "1 active instance"
        } else {
            "No running instances"
        },
        rect(32.0, y + 46.0, card_w - 52.0, 18.0),
        mtm,
        TextStyle::Caption,
    );
    add_hit_button(
        parent,
        card_frame,
        index,
        handler,
        sel!(selectContainerSession:),
        mtm,
    );

    let primary_label = if process_active {
        "Open"
    } else if launch_busy {
        state.as_ref().map(session_state_label).unwrap_or("Running")
    } else if transport_blocked {
        "Blocked"
    } else if missing_image {
        if is_smoke_image_reference(&session.image) {
            "Build"
        } else if is_local_image_reference(&session.image) {
            "Load OCI"
        } else {
            "Pull"
        }
    } else {
        "Launch"
    };
    let primary_selector = if process_active {
        sel!(selectContainerSession:)
    } else if launch_busy || transport_blocked {
        sel!(checkContainerSession:)
    } else if missing_image {
        if is_smoke_image_reference(&session.image) {
            sel!(buildSmokeContainerSessionImage:)
        } else if is_local_image_reference(&session.image) {
            sel!(loadContainerSessionImage:)
        } else {
            sel!(pullContainerSessionImage:)
        }
    } else {
        sel!(launchContainerSession:)
    };
    let primary_tooltip = if process_active {
        "Open this application profile and inspect its running instance"
    } else if launch_busy {
        "This application is starting or stopping"
    } else if transport_blocked {
        "Apple Container GUI relay is currently unavailable"
    } else if missing_image {
        if is_smoke_image_reference(&session.image) {
            "Build the bundled example image with Apple Container before launching"
        } else if is_local_image_reference(&session.image) {
            "Load an OCI archive into Apple Container before launching"
        } else {
            "Pull the missing image before launching"
        }
    } else {
        "Launch an application instance from this saved profile"
    };
    let primary = add_button(
        parent,
        primary_label,
        rect(32.0, y + 14.0, 82.0, 28.0),
        handler,
        primary_selector,
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*primary, setTag: index as isize];
        let _: () = msg_send![&*primary, setToolTip:
            &*NSString::from_str(primary_tooltip)];
        if !process_active && (launch_busy || transport_blocked) {
            let _: () = msg_send![&*primary, setEnabled: false];
        }
    }

    if process_active || matches!(state_label, "Running" | "Stopping") {
        let force_stop = state
            .as_ref()
            .is_some_and(|state| state.force_stop_available);
        let stop = add_button(
            parent,
            if force_stop {
                "Force Stop"
            } else {
                "Stop Instance"
            },
            rect(
                124.0,
                y + 14.0,
                if force_stop { 100.0 } else { 112.0 },
                28.0,
            ),
            handler,
            if force_stop {
                sel!(forceStopContainerSession:)
            } else {
                sel!(stopContainerSession:)
            },
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*stop, setTag: index as isize];
            let _: () = msg_send![&*stop, setToolTip:
            &*NSString::from_str(if force_stop {
                "Immediately terminate an application that did not stop gracefully"
            } else {
                "Ask the application to exit gracefully"
            })];
        }
    }

    let more = add_popup(
        parent,
        rect(card_w - 58.0, y + 14.0, 46.0, 28.0),
        &[
            "…",
            "Duplicate Profile",
            "Export Profile",
            "View Raw Configuration",
            "Delete Profile",
        ],
        0,
        mtm,
    );
    unsafe {
        let _: () = msg_send![&*more, setTarget: handler];
        let _: () = msg_send![&*more, setAction: sel!(applicationProfileMoreAction:)];
        let _: () = msg_send![&*more, setTag: index as isize];
        let _: () = msg_send![&*more, setToolTip:
            &*NSString::from_str("More profile actions")];
    }
}

fn session_display_command(session: &ContainerSession) -> String {
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

fn session_display_target(session: &ContainerSession) -> String {
    session
        .display
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("auto")
        .to_string()
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

fn resolved_session_display_target(session: &ContainerSession) -> &'static str {
    match session_display_target(session).as_str() {
        "auto" => "automatic",
        "default" => "default",
        _ => "dedicated",
    }
}

fn session_display_summary(session: &ContainerSession) -> String {
    let requested = session_display_target(session);
    match requested.as_str() {
        "auto" | "default" | "dedicated" => requested,
        _ => format!("{} -> dedicated", requested),
    }
}

fn session_presentation_summary(session: &ContainerSession) -> &'static str {
    if session.presentation_mode().is_rootless() {
        "Rootless"
    } else {
        "Desktop"
    }
}

fn session_waypipe_summary(session: &ContainerSession) -> String {
    let compress = session
        .waypipe_compress
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if container_sessions::is_apple_container_session(session) {
                "none"
            } else {
                "lz4"
            }
        });
    let threads = session
        .waypipe_threads
        .map(|value| value.to_string())
        .unwrap_or_else(|| "auto".into());
    format!("compress {}; threads {}", compress, threads)
}

fn short_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.into();
    }
    let mut result = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    result.push_str("...");
    result
}

fn chars_for_width(width: f64, style: TextStyle) -> usize {
    let average_char_width = match style {
        TextStyle::Hero => 15.0,
        TextStyle::Title => 12.0,
        TextStyle::Heading => 8.5,
        TextStyle::Section | TextStyle::Caption => 6.5,
        TextStyle::Body => 7.2,
        TextStyle::Mono => 7.8,
    };
    ((width.max(32.0) / average_char_width).floor() as usize).max(8)
}

fn wrapped_log_lines(logs: &[String], max_chars: usize, max_rows: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in logs {
        let mut remaining = line.as_str();
        let mut first = true;
        while !remaining.is_empty() {
            let prefix = if first { "" } else { "  " };
            let limit = max_chars.saturating_sub(prefix.chars().count()).max(16);
            let (chunk, rest) = split_at_char_count(remaining, limit);
            rows.push(format!("{}{}", prefix, chunk));
            remaining = rest.trim_start();
            first = false;
        }
    }

    if rows.len() > max_rows {
        rows[rows.len() - max_rows..].to_vec()
    } else {
        rows
    }
}

fn split_at_char_count(value: &str, max_chars: usize) -> (&str, &str) {
    if value.chars().count() <= max_chars {
        return (value, "");
    }
    let split = value
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    value.split_at(split)
}

fn short_image_label(image: &str, max_chars: usize) -> String {
    short_text(image, max_chars)
}

fn runtime_label(runtime: &str) -> &'static str {
    match runtime.trim().to_ascii_lowercase().as_str() {
        "docker" => "Docker",
        "orb" | "orbstack" => "OrbStack",
        _ => "Apple Container",
    }
}

fn runtime_nav(runtime: &str) -> usize {
    match runtime.trim().to_ascii_lowercase().as_str() {
        "docker" => NAV_DOCKER,
        "orb" | "orbstack" => NAV_ORBSTACK,
        _ => NAV_APPLE_CONTAINER,
    }
}

fn request_selected_runtime_container_details() {
    let selected = SELECTED_RUNTIME_CONTAINER.lock().unwrap().clone();
    let Some(selected) = selected else {
        return;
    };
    *RUNTIME_CONTAINER_DETAILS.lock().unwrap() = None;
    send(CompositorMessage::RefreshRuntimeContainerDetails {
        runtime: selected.runtime,
        name: selected.name,
    });
}

#[derive(Clone, Copy)]
enum TextStyle {
    Hero,
    Title,
    Heading,
    Section,
    Body,
    Caption,
    Mono,
}

fn add_label(
    parent: &NSView,
    text: &str,
    frame: NSRect,
    mtm: MainThreadMarker,
    style: TextStyle,
) -> Retained<NSTextField> {
    unsafe {
        let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        label.setFrame(frame);
        label.setSelectable(false);
        let font = match style {
            TextStyle::Hero => NSFont::boldSystemFontOfSize(28.0),
            TextStyle::Title => NSFont::boldSystemFontOfSize(22.0),
            TextStyle::Heading => NSFont::boldSystemFontOfSize(15.0),
            TextStyle::Section => NSFont::boldSystemFontOfSize(12.0),
            TextStyle::Body => NSFont::systemFontOfSize(13.0),
            TextStyle::Caption => NSFont::systemFontOfSize(11.0),
            TextStyle::Mono => NSFont::userFixedPitchFontOfSize(13.0)
                .unwrap_or_else(|| NSFont::systemFontOfSize(13.0)),
        };
        let _: () = msg_send![&*label, setFont: &*font];
        // Headings must stay on one line; treating every tall label as multiline
        // lets AppKit wrap titles into neighbouring fixed-layout rows.
        let multiline_style = matches!(
            style,
            TextStyle::Body | TextStyle::Caption | TextStyle::Mono
        );
        let multiline = text.contains('\n')
            || (multiline_style && frame.size.height >= line_height_for_style(style) * 1.6);
        let line_break_mode: isize = if multiline { 0 } else { 4 };
        let line_height = line_height_for_style(style);
        let max_lines: isize = if multiline {
            (frame.size.height / line_height).floor().max(1.0) as isize
        } else {
            1
        };
        let _: () = msg_send![&*label, setUsesSingleLineMode: !multiline];
        let _: () = msg_send![&*label, setPreferredMaxLayoutWidth: frame.size.width];
        let _: () = msg_send![&*label, setLineBreakMode: line_break_mode];
        let _: () = msg_send![&*label, setMaximumNumberOfLines: max_lines];
        let cell: Option<Retained<AnyObject>> = msg_send_id![&*label, cell];
        if let Some(cell) = cell {
            let _: () = msg_send![&*cell, setWraps: multiline];
            let _: () = msg_send![&*cell, setScrollable: false];
        }
        let visible_chars =
            chars_for_width(frame.size.width, style).saturating_mul(max_lines.max(1) as usize);
        if text.chars().count() > visible_chars {
            let _: () = msg_send![&*label, setToolTip: &*NSString::from_str(text)];
        }
        parent.addSubview(&label);
        label
    }
}

fn line_height_for_style(style: TextStyle) -> f64 {
    match style {
        TextStyle::Hero => 34.0,
        TextStyle::Title => 27.0,
        TextStyle::Heading => 20.0,
        TextStyle::Section => 17.0,
        TextStyle::Body | TextStyle::Mono => 18.0,
        TextStyle::Caption => 15.0,
    }
}

fn add_button(
    parent: &NSView,
    title: &str,
    frame: NSRect,
    handler: *mut AnyObject,
    action: objc2::runtime::Sel,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    unsafe {
        let button = NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(&*handler),
            Some(action),
            mtm,
        );
        button.setFrame(frame);
        parent.addSubview(&button);
        button
    }
}

fn add_text_field(
    parent: &NSView,
    frame: NSRect,
    placeholder: &str,
    value: &str,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    unsafe {
        let field: Retained<NSTextField> =
            msg_send_id![mtm.alloc::<NSTextField>(), initWithFrame: frame];
        let _: () = msg_send![&*field, setPlaceholderString:
            &*NSString::from_str(placeholder)];
        if !value.is_empty() {
            let _: () = msg_send![&*field, setStringValue:
                &*NSString::from_str(value)];
        }
        parent.addSubview(&field);
        field
    }
}

fn add_secure_text_field(
    parent: &NSView,
    frame: NSRect,
    placeholder: &str,
    mtm: MainThreadMarker,
) -> Retained<NSSecureTextField> {
    unsafe {
        let field: Retained<NSSecureTextField> =
            msg_send_id![mtm.alloc::<NSSecureTextField>(), initWithFrame: frame];
        let _: () = msg_send![&*field, setPlaceholderString:
            &*NSString::from_str(placeholder)];
        parent.addSubview(&field);
        field
    }
}

fn add_popup(
    parent: &NSView,
    frame: NSRect,
    items: &[&str],
    selected: usize,
    mtm: MainThreadMarker,
) -> Retained<NSPopUpButton> {
    unsafe {
        let popup: Retained<NSPopUpButton> = msg_send_id![
            mtm.alloc::<NSPopUpButton>(),
            initWithFrame: frame,
            pullsDown: false
        ];
        for item in items {
            let _: () = msg_send![&*popup, addItemWithTitle: &*NSString::from_str(item)];
        }
        let _: () = msg_send![&*popup, selectItemAtIndex: selected as isize];
        parent.addSubview(&popup);
        popup
    }
}

fn add_hit_button(
    parent: &NSView,
    frame: NSRect,
    tag: usize,
    handler: *mut AnyObject,
    action: objc2::runtime::Sel,
    mtm: MainThreadMarker,
) {
    unsafe {
        let button = NSButton::buttonWithTitle_target_action(
            &NSString::from_str(""),
            Some(&*handler),
            Some(action),
            mtm,
        );
        button.setFrame(frame);
        button.setBordered(false);
        button.setTransparent(true);
        let _: () = msg_send![&*button, setTag: tag as isize];
        parent.addSubview(&button);
    }
}

fn add_card(parent: &NSView, frame: NSRect, mtm: MainThreadMarker) {
    unsafe {
        let card = NSBox::initWithFrame(mtm.alloc::<NSBox>(), frame);
        card.setBoxType(NSBoxType::NSBoxCustom);
        card.setTitle(&NSString::from_str(""));
        card.setTransparent(false);
        card.setCornerRadius(10.0);
        card.setBorderWidth(0.5);
        let fill = NSColor::controlBackgroundColor().colorWithAlphaComponent(0.62);
        let border = NSColor::separatorColor().colorWithAlphaComponent(0.55);
        card.setFillColor(&fill);
        card.setBorderColor(&border);
        parent.addSubview(&card);
    }
}

fn add_profile_card(
    parent: &NSView,
    frame: NSRect,
    selected: bool,
    runtime_nav: usize,
    mtm: MainThreadMarker,
) {
    if !selected {
        add_card(parent, frame, mtm);
        return;
    }
    unsafe {
        let card = NSBox::initWithFrame(mtm.alloc::<NSBox>(), frame);
        card.setBoxType(NSBoxType::NSBoxCustom);
        card.setTitle(&NSString::from_str(""));
        card.setTransparent(false);
        card.setCornerRadius(10.0);
        card.setBorderWidth(1.5);
        let accent = NSColor::controlAccentColor();
        card.setFillColor(&accent.colorWithAlphaComponent(0.09));
        card.setBorderColor(&accent.colorWithAlphaComponent(0.75));
        parent.addSubview(&card);
    }
    add_runtime_accent(
        parent,
        runtime_nav,
        rect(
            frame.origin.x,
            frame.origin.y + 10.0,
            4.0,
            frame.size.height - 20.0,
        ),
        mtm,
    );
}

fn add_runtime_accent(parent: &NSView, nav: usize, frame: NSRect, mtm: MainThreadMarker) {
    unsafe {
        let accent = NSBox::initWithFrame(mtm.alloc::<NSBox>(), frame);
        accent.setBoxType(NSBoxType::NSBoxCustom);
        accent.setTitle(&NSString::from_str(""));
        accent.setTransparent(false);
        accent.setCornerRadius(frame.size.width / 2.0);
        accent.setBorderWidth(0.0);
        let color = match nav {
            NAV_APPLE_CONTAINER => NSColor::systemBlueColor(),
            NAV_DOCKER => NSColor::systemTealColor(),
            NAV_ORBSTACK => NSColor::systemOrangeColor(),
            _ => NSColor::controlAccentColor(),
        };
        accent.setFillColor(&color.colorWithAlphaComponent(0.9));
        parent.addSubview(&accent);
    }
}

fn add_separator(parent: &NSView, frame: NSRect, mtm: MainThreadMarker) {
    unsafe {
        let sep = NSBox::initWithFrame(mtm.alloc::<NSBox>(), frame);
        sep.setBoxType(NSBoxType::NSBoxSeparator);
        parent.addSubview(&sep);
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect {
        origin: NSPoint { x, y },
        size: NSSize { width, height },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_sources_build_canonical_image_references() {
        assert_eq!(
            normalize_registry_reference(0, "ubuntu:24.04"),
            "docker.io/library/ubuntu:24.04"
        );
        assert_eq!(
            normalize_registry_reference(0, "library/alpine:3.20"),
            "docker.io/library/alpine:3.20"
        );
        assert_eq!(
            normalize_registry_reference(1, "owner/desktop:latest"),
            "ghcr.io/owner/desktop:latest"
        );
        assert_eq!(
            normalize_registry_reference(2, "owner/desktop:latest"),
            "quay.io/owner/desktop:latest"
        );
    }

    #[test]
    fn explicit_registry_reference_is_not_prefixed_twice() {
        assert_eq!(
            normalize_registry_reference(0, "registry.example.com/team/gui:v1"),
            "registry.example.com/team/gui:v1"
        );
        assert_eq!(
            normalize_registry_reference(3, "localhost/gui:latest"),
            "localhost/gui:latest"
        );
    }

    #[test]
    fn generic_images_do_not_assume_a_terminal_command() {
        let defaults = session_defaults_for_image("container", "docker.io/library/ubuntu:24.04");
        assert_eq!(defaults.runtime, "container");
        assert_eq!(defaults.profile, "single-app");
        assert!(defaults.command.is_empty());

        let niri = session_defaults_for_image("container", "example/cocoa-way-niri:latest");
        assert_eq!(niri.profile, "niri");
        assert_eq!(niri.command, "niri");
    }

    #[test]
    fn clean_session_log_line_strips_ansi_sequences() {
        assert_eq!(
            clean_session_log_line(
                "\u{1b}[2m2026-06-26\u{1b}[0m \u{1b}[33mWARN\u{1b}[0m [2mniri[0m"
            ),
            "2026-06-26 WARN niri"
        );
    }

    #[test]
    fn clean_session_log_line_marks_niri_locale_warning_non_fatal() {
        let line = "\u{1b}[2m2026-06-26T12:39:42Z\u{1b}[0m \u{1b}[33mWARN\u{1b}[0m \u{1b}[2mniri::dbus\u{1b}[0m: error starting locale1 watcher: I/O error: No such file or directory (os error 2)";
        assert_eq!(
            clean_session_log_line(line),
            "niri: locale1 watcher is unavailable in this container; this is non-fatal when the desktop is running."
        );
    }

    #[test]
    fn apple_container_row_keeps_lifecycle_fields() {
        let row = "cocoa-way-niri-desktop localhost/cocoa-way-niri:latest linux arm64 running 192.168.64.66/24 4 1024 MB 2026-06-25T01:33:46Z";
        let parsed = parse_apple_container_row(row);
        assert_eq!(parsed.name.as_deref(), Some("cocoa-way-niri-desktop"));
        assert!(parsed.running);
        assert!(parsed.label.contains("localhost/cocoa-way-niri:latest"));
    }

    #[test]
    fn apple_container_row_protects_buildkit_helper() {
        let row = "buildkit builder:latest linux arm64 stopped - 2 2048 MB 2026-07-13T05:13:37Z";
        let parsed = parse_apple_container_row(row);
        assert_eq!(parsed.name, None);
        assert!(!parsed.running);
        assert!(parsed.label.contains("BuildKit helper"));
    }

    #[test]
    fn format_docker_container_row_keeps_actions_and_summary() {
        let running = format_docker_container_row("web\trunning\tUp 2 minutes\tnginx:latest");
        assert_eq!(running.name.as_deref(), Some("web"));
        assert!(running.running);
        assert!(running.label.contains("nginx:latest"));

        let stopped =
            format_docker_container_row("worker\texited\tExited (0) 1 hour ago\talpine:latest");
        assert_eq!(stopped.name.as_deref(), Some("worker"));
        assert!(!stopped.running);
    }

    #[test]
    fn wrapped_log_lines_respects_row_limit() {
        let logs = vec!["abcdefghijklmnopqrstuvwxyz".to_string()];
        let rows = wrapped_log_lines(&logs, 16, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "abcdefghijklmnop");
        assert_eq!(rows[1], "  qrstuvwxyz");
    }

    #[test]
    fn long_session_detail_values_reserve_a_second_line() {
        assert_eq!(session_detail_row_height("Running", 240.0), 26.0);
        assert_eq!(
            session_detail_row_height(
                "Tracked by Cocoa-Way with a long transport and display occupancy summary",
                160.0,
            ),
            42.0
        );
    }

    #[test]
    fn desktop_sessions_receive_larger_apple_container_limits() {
        assert_eq!(
            default_gui_runtime_args("container", Some("niri")),
            ["--memory", "4G", "--shm-size", "1G", "--cpus", "4"]
        );
    }

    #[test]
    fn untracked_runtime_instance_does_not_disable_profile_launch() {
        assert_eq!(checked_instance_status(true, false), None);
        assert_eq!(
            checked_instance_status(true, true),
            Some(InstanceStatus::Running)
        );
    }

    #[test]
    fn single_apps_keep_moderate_apple_container_limits() {
        assert_eq!(
            default_gui_runtime_args("container", Some("single-app")),
            ["--memory", "2G", "--shm-size", "512M", "--cpus", "4"]
        );
        assert!(default_gui_runtime_args("docker", Some("niri")).is_empty());
    }

    #[test]
    fn docker_image_inventory_preserves_reference_and_metadata() {
        let row = parse_docker_image_line("alpine:3.20\tdeadbeef\t8MB").unwrap();
        assert_eq!(row.reference.as_deref(), Some("alpine:3.20"));
        assert!(row.label.contains("deadbeef"));
        assert!(row.label.contains("8MB"));
    }

    #[test]
    fn dangling_docker_image_uses_id_for_actions() {
        let row = parse_docker_image_line("<none>:<none>\tcafebabe\t12MB").unwrap();
        assert_eq!(row.reference.as_deref(), Some("cafebabe"));
    }

    #[test]
    fn image_reference_split_preserves_registry_ports() {
        assert_eq!(
            split_image_reference("localhost:5000/team/desktop:v2"),
            ("localhost:5000/team/desktop", "v2")
        );
        assert_eq!(split_image_reference("ubuntu:24.04"), ("ubuntu", "24.04"));
        assert_eq!(
            split_image_reference("sha256:deadbeef"),
            ("sha256", "deadbeef")
        );
        assert_eq!(
            split_image_reference("untagged-image"),
            ("untagged-image", "<none>")
        );
    }

    #[test]
    fn image_id_is_read_from_inventory_metadata() {
        assert_eq!(
            image_id_from_label("ubuntu:24.04    deadbeef    78MB", "ubuntu:24.04"),
            Some("deadbeef".into())
        );
        assert_eq!(image_id_from_label("ubuntu:24.04", "ubuntu:24.04"), None);
    }

    #[test]
    fn runtime_arguments_support_split_and_equals_values() {
        let arguments = vec![
            "--memory".into(),
            "4G".into(),
            "--cpus=4".into(),
            "--read-only".into(),
        ];
        assert_eq!(runtime_arg_value(&arguments, "--memory"), Some("4G".into()));
        assert_eq!(runtime_arg_value(&arguments, "--cpus"), Some("4".into()));
        assert_eq!(
            format_runtime_arguments(&arguments),
            "--memory 4G\n--cpus=4\n--read-only"
        );
    }

    #[test]
    fn docker_volume_inventory_keeps_name_and_driver() {
        let row = parse_volume_line("project-cache\tlocal").unwrap();
        assert_eq!(row.name.as_deref(), Some("project-cache"));
        assert!(row.label.contains("local"));
    }

    #[test]
    fn volume_mount_matching_accepts_runtime_mount_syntax() {
        assert!(mount_references_volume(
            "type=volume,source=project-data,target=/workspace",
            "project-data"
        ));
        assert!(mount_references_volume(
            "project-data:/workspace",
            "project-data"
        ));
        assert!(!mount_references_volume(
            "type=bind,source=/tmp/project-data,target=/workspace",
            "project-data"
        ));
    }

    #[test]
    fn apple_container_volume_usage_reads_mount_sources() {
        let json = r#"[
            {"id":"desktop","configuration":{"mounts":[{"source":"project-data","type":{"volume":{}}}]}},
            {"id":"other","configuration":{"mounts":[]}}
        ]"#;
        assert_eq!(
            parse_apple_volume_mounts(json, "project-data").unwrap(),
            ["desktop"]
        );
    }

    #[test]
    fn volume_inspect_metadata_supports_docker_and_apple_fields() {
        let docker = br#"[{"Driver":"local","CreatedAt":"2026-07-16T12:00:00Z"}]"#;
        let docker = parse_volume_inspect_metadata(docker, "Managed volume").unwrap();
        assert_eq!(docker.kind, "local");
        assert_eq!(docker.created, "2026-07-16T12:00:00Z");

        let apple =
            br#"[{"type":"ext4","sizeInBytes":1073741824,"creationDate":"2026-07-16T12:00:00Z"}]"#;
        let apple = parse_volume_inspect_metadata(apple, "Managed volume").unwrap();
        assert_eq!(apple.kind, "ext4");
        assert_eq!(apple.size, "1.0 GiB");
    }

    #[test]
    fn docker_context_inventory_marks_current_context() {
        let row = parse_docker_context_line(
            "orbstack\ttrue\tunix:///Users/test/.orbstack/run/docker.sock\tOrbStack",
        )
        .unwrap();
        assert_eq!(row.name.as_deref(), Some("orbstack"));
        assert!(row.current);
        assert!(row.label.starts_with("* orbstack"));
    }

    #[test]
    fn orbstack_machine_inventory_parses_json_and_state() {
        let rows = parse_orbstack_machine_rows(
            r#"[{"name":"arch","image":{"distro":"archlinux","version":"current","arch":"arm64"},"state":"stopped"},{"name":"ubuntu","image":{"distro":"ubuntu","version":"24.04","arch":"arm64"},"state":"running"}]"#,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name.as_deref(), Some("arch"));
        assert!(rows[0].detail.contains("archlinux current"));
        assert!(!rows[0].running);
        assert_eq!(rows[1].name.as_deref(), Some("ubuntu"));
        assert!(rows[1].running);
    }

    #[test]
    fn apple_container_versions_are_extracted_from_cli_and_api_text() {
        assert_eq!(
            extract_version("container CLI version 1.1.0 (build: release)"),
            Some("1.1.0".into())
        );
        assert_eq!(
            extract_version("container-apiserver version 1.0.0 (build: release)"),
            Some("1.0.0".into())
        );
        assert_eq!(
            extract_version("container CLI version 1.3.1 (build: release)"),
            Some("1.3.1".into())
        );
    }

    #[test]
    fn apple_container_version_comparison_handles_minor_updates() {
        assert!(version_at_least(
            "1.1.0",
            APPLE_CONTAINER_TRANSPORT_V2_MINIMUM
        ));
        assert!(version_at_least("1.3.1", APPLE_CONTAINER_SECURITY_BASELINE));
        assert!(version_at_least("2.0.0", APPLE_CONTAINER_SECURITY_BASELINE));
        assert!(!version_at_least(
            "1.3.0",
            APPLE_CONTAINER_SECURITY_BASELINE
        ));
        assert!(!version_at_least("unknown", (1, 0, 0)));
    }

    #[test]
    fn slow_ui_commands_complete_in_the_background() {
        invalidate_ui_command_cache();
        let started = Instant::now();
        let first = run_ui_command(
            std::path::Path::new("/bin/sh"),
            "/usr/bin:/bin",
            &["-c", "sleep 0.15; printf ready"],
            Duration::from_secs(1),
        );
        assert_eq!(first.unwrap_err(), UI_COMMAND_LOADING);
        assert!(started.elapsed() < Duration::from_millis(100));

        let output = (0..20)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(25));
                run_ui_command(
                    std::path::Path::new("/bin/sh"),
                    "/usr/bin:/bin",
                    &["-c", "sleep 0.15; printf ready"],
                    Duration::from_secs(1),
                )
                .ok()
            })
            .expect("background command should populate the UI cache");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready");
    }
}
