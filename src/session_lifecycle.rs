//! Session lifecycle owns process handles, never native windows or renderers.
//! Only nonblocking try_wait/SIGTERM runs on the native thread; all joins, CLI
//! inspection/deletion and directory removal run through CleanupQueue. Typed
//! resource bundles retain all leases across failures and cleanup retries.

use super::{ActiveContainerSession, DisplayWorker, ManagedDisplay, PendingProfileOperation};
use crate::container_sessions::ProfileId;
use crate::{container_mode, container_sessions, lifecycle};

pub(super) fn terminate_display_worker(worker: &mut DisplayWorker) -> Result<(), String> {
    terminate_child(&mut worker.child)?;
    remove_runtime_directory(&mut worker.runtime_dir)
}

pub(super) fn terminate_child(child: &mut std::process::Child) -> Result<(), String> {
    match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            if let Err(error) = child.kill() {
                // Exiting between try_wait and kill is still successful cleanup.
                return match child.try_wait() {
                    Ok(Some(_)) => Ok(()),
                    _ => Err(error.to_string()),
                };
            }
            child.wait().map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The native caller relinquishes the handle without waiting for process exit.
pub(super) fn queue_classic_child(
    mut child: std::process::Child,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    if let Err(error) = request_child_exit(&mut child) {
        log::warn!(
            "Classic child {} exit request failed; scheduling cleanup: {error}",
            child.id()
        );
    }
    cleanup.enqueue(
        lifecycle::Key::Abandoned,
        child,
        terminate_child,
        |child, result| {
            if let Err(error) = result {
                log::warn!(
                    "Classic child {} cleanup failed; retaining handle for retry: {error}",
                    child.id()
                );
            }
        },
    );
}

fn remove_runtime_directory(path: &mut std::path::PathBuf) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("Refusing to remove an empty runtime directory path".into());
    }
    std::fs::remove_dir_all(&*path)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| error.to_string())
}

/// Only pass a captured, owned runtime directory after its producer has exited.
/// This does not resolve a directory from a mutable profile or display slot.
pub(super) fn queue_runtime_directory_cleanup(
    path: std::path::PathBuf,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    cleanup.enqueue(
        lifecycle::Key::Abandoned,
        path,
        remove_runtime_directory,
        |path, result| {
            if let Err(error) = result {
                log::warn!(
                    "Runtime directory {} cleanup failed; retaining job for retry: {error}",
                    path.display()
                );
            }
        },
    );
}

struct TypedRollback<T> {
    resources: T,
    _producer: crate::lifecycle_producers::Lease,
    kind: &'static str,
    cleanup: fn(&mut T) -> Result<(), String>,
}

fn rollback_disconnected_owned<T: Send + 'static>(
    resources: T,
    producer: crate::lifecycle_producers::Lease,
    kind: &'static str,
    cleanup: fn(&mut T) -> Result<(), String>,
    timeout: std::time::Duration,
) {
    let mut queue = lifecycle::CleanupQueue::new(|| {});
    queue.enqueue(
        lifecycle::Key::Abandoned,
        TypedRollback { resources, _producer: producer, kind, cleanup },
        |rollback| (rollback.cleanup)(&mut rollback.resources),
        |rollback, result| {
            if let Err(error) = result {
                log::warn!(
                    "Disconnected {} rollback failed; retaining resources and producer lease for retry: {error}",
                    rollback.kind,
                );
            }
        },
    );
    if !queue.finish(timeout) {
        log::warn!(
            "Disconnected {kind} rollback exceeded its local deadline; transferring owned resources and producer lease to the background reaper"
        );
    }
    // Queue Drop hands off the entire envelope, never just its process handles.
}

/// Background producer only. A failed or timed-out rollback keeps the producer
/// counted through retries; no native/UI completion callback is invoked.
pub(super) fn rollback_disconnected_display(
    worker: DisplayWorker,
    producer: crate::lifecycle_producers::Lease,
) {
    rollback_disconnected_owned(
        worker,
        producer,
        "display",
        terminate_display_worker,
        std::time::Duration::from_secs(15),
    );
}

/// Uses only the returned report's immutable cleanup authority. Its producer
/// lease moves with the report until cleanup and background destruction succeed.
pub(super) fn rollback_disconnected_launch(
    report: container_sessions::LaunchReport,
    producer: crate::lifecycle_producers::Lease,
) {
    rollback_disconnected_owned(
        report,
        producer,
        "launch",
        cleanup_launch_report,
        std::time::Duration::from_secs(15),
    );
}

pub(super) fn request_child_exit(child: &mut std::process::Child) -> Result<(), String> {
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

pub(super) fn request_graceful_container_stop(
    active: &mut [ActiveContainerSession],
    index: ProfileId,
) -> Result<(), String> {
    let Some(session) = active
        .iter_mut()
        .find(|session| session.profile_id == index)
    else {
        return Err("No active instance is tracked for this profile".into());
    };
    if session.stopping_since.is_some() {
        return Ok(());
    }
    if let Some(audio_worker) = session.audio_worker.as_mut() {
        audio_worker.request_stop();
    }
    session.stopping_since = Some(std::time::Instant::now());
    session.force_stop_offered = false;
    let mut errors = Vec::new();
    collect_error(
        &mut errors,
        "Waypipe exit request",
        request_child_exit(&mut session.waypipe_child),
    );
    if let Some(container_child) = session.container_child.as_mut() {
        collect_error(
            &mut errors,
            "container exit request",
            request_child_exit(container_child),
        );
    }
    cleanup_result(errors)
}

pub(super) fn stop_active_container_session(
    active: &mut Vec<ActiveContainerSession>,
    index: ProfileId,
    cleanup: &mut lifecycle::CleanupQueue,
) -> Result<(), String> {
    let Some(position) = active
        .iter()
        .position(|session| session.profile_id == index)
    else {
        return Err("No active process is tracked for this session".into());
    };
    let mut session = active.remove(position);
    session
        .stopping_since
        .get_or_insert_with(std::time::Instant::now);
    queue_session_cleanup(session, "application", "force stopped".into(), cleanup);
    Ok(())
}

pub(super) fn cleanup_session_resources(
    session: &mut ActiveContainerSession,
) -> Result<(), String> {
    if let Some(audio_worker) = session.audio_worker.as_ref() {
        audio_worker.request_stop();
    }
    let mut errors = Vec::new();
    collect_error(
        &mut errors,
        "Waypipe",
        terminate_child(&mut session.waypipe_child),
    );
    if let Some(child) = session.container_child.as_mut() {
        collect_error(&mut errors, "container process", terminate_child(child));
    }
    if let Some(worker) = session.display_worker.as_mut() {
        collect_error(
            &mut errors,
            "dedicated display",
            terminate_display_worker(worker),
        );
    }
    collect_error(
        &mut errors,
        "owned container",
        cleanup_owned_container_session(&session.owned_identity),
    );
    // A slow audio join cannot prevent the other owned resources receiving stop.
    if let Some(audio_worker) = session.audio_worker.as_mut() {
        audio_worker.stop();
    }
    cleanup_result(errors)
}

fn collect_error(errors: &mut Vec<String>, resource: &str, result: Result<(), String>) {
    if let Err(error) = result {
        errors.push(format!("{resource}: {error}"));
    }
}

fn cleanup_result(errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

struct SessionCleanup {
    session: ActiveContainerSession,
    process: &'static str,
    status: String,
}

fn complete_session_cleanup(resources: &SessionCleanup, result: Result<(), String>) {
    let session = &resources.session;
    finish_reaped_container_session(
        session.profile_id,
        resources.process,
        &resources.status,
        session.stopping_since.is_some(),
        session.display_worker.is_some(),
        result,
    );
}

pub(super) fn queue_session_cleanup(
    session: ActiveContainerSession,
    process: &'static str,
    status: String,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    let key = lifecycle::Key::Session(session.profile_id, session.display_slot.clone());
    if let Some(audio_worker) = session.audio_worker.as_ref() {
        audio_worker.request_stop();
    }
    cleanup.enqueue(
        key,
        SessionCleanup {
            session,
            process,
            status,
        },
        |resources| cleanup_session_resources(&mut resources.session),
        complete_session_cleanup,
    );
}

struct DisplayCleanup {
    worker: DisplayWorker,
    slot: String,
    exit_reason: Option<String>,
    failed_launch: Option<ProfileId>,
    _reservation: Option<crate::profile_activity::RuntimeReservation>,
    _activity: Option<crate::profile_activity::ProfileActivity>,
}

fn complete_display_cleanup(resources: &DisplayCleanup, result: Result<(), String>) {
    match result {
        Ok(()) => {
            if let Some(reason) = &resources.exit_reason {
                container_mode::record_managed_display_exit(&resources.slot, reason);
            }
        }
        Err(error) => {
            if let Some(id) = resources.failed_launch {
                container_mode::record_stop_failure(
                    id,
                    &format!(
                        "Dedicated display cleanup failed; resources remain reserved for retry: {error}"
                    ),
                );
            } else if resources.exit_reason.is_some() {
                container_mode::record_managed_display_failure(&resources.slot, &error);
            } else {
                log::warn!(
                    "Display cleanup for '{}' failed; retaining ownership: {error}",
                    resources.slot
                );
            }
        }
    }
}

fn enqueue_display_cleanup(resources: DisplayCleanup, cleanup: &mut lifecycle::CleanupQueue) {
    let key = match resources.failed_launch {
        Some(id) => lifecycle::Key::Session(id, resources.slot.clone()),
        None => lifecycle::Key::Display(resources.slot.clone()),
    };
    cleanup.enqueue(
        key,
        resources,
        |resources| terminate_display_worker(&mut resources.worker),
        complete_display_cleanup,
    );
}

pub(super) fn queue_display_cleanup(
    worker: DisplayWorker,
    slot: String,
    notify: bool,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    enqueue_display_cleanup(
        DisplayCleanup {
            worker,
            slot,
            exit_reason: notify.then(|| "closed by user".into()),
            failed_launch: None,
            _reservation: None,
            _activity: None,
        },
        cleanup,
    );
}

/// Replaces main's owning cleanup closure on the failed dedicated-launch path.
pub(super) fn queue_failed_launch_display_cleanup(
    id: ProfileId,
    slot: String,
    worker: DisplayWorker,
    reservation: Option<crate::profile_activity::RuntimeReservation>,
    activity: crate::profile_activity::ProfileActivity,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    enqueue_display_cleanup(
        DisplayCleanup {
            worker,
            slot,
            exit_reason: None,
            failed_launch: Some(id),
            _reservation: reservation,
            _activity: Some(activity),
        },
        cleanup,
    );
}

struct LaunchCleanup {
    id: ProfileId,
    report: Option<container_sessions::LaunchReport>,
    worker: Option<DisplayWorker>,
    operation: Option<PendingProfileOperation>,
    _activity: crate::profile_activity::ProfileActivity,
}

fn cleanup_launch_report(report: &mut container_sessions::LaunchReport) -> Result<(), String> {
    let mut errors = Vec::new();
    collect_error(
        &mut errors,
        "Waypipe",
        terminate_child(&mut report.waypipe_child),
    );
    if let Some(child) = report.container_child.as_mut() {
        collect_error(&mut errors, "container process", terminate_child(child));
    }
    collect_error(
        &mut errors,
        "owned container",
        cleanup_owned_container_session(&report.identity),
    );
    cleanup_result(errors)
}

fn cleanup_launch_resources(resources: &mut LaunchCleanup) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Some(report) = resources.report.as_mut()
        && let Err(error) = cleanup_launch_report(report)
    {
        errors.push(error);
    }
    if let Some(worker) = resources.worker.as_mut() {
        collect_error(
            &mut errors,
            "dedicated display",
            terminate_display_worker(worker),
        );
    }
    cleanup_result(errors)
}

fn complete_launch_cleanup(resources: &LaunchCleanup, result: Result<(), String>) {
    // Stale results own their own resources but must not change a newer attempt's UI.
    let Some(operation) = &resources.operation else {
        return;
    };
    match result {
        Ok(()) if operation.cancelled => container_mode::record_launch_cancelled(
            resources.id,
            "Launch cancelled; owned runtime and display resources were released.",
        ),
        Err(error) => container_mode::record_stop_failure(
            resources.id,
            &format!("Launch rollback failed; resources remain reserved for retry: {error}"),
        ),
        _ => {}
    }
}

pub(super) fn queue_launch_cleanup(
    index: ProfileId,
    display_slot: String,
    report: Option<container_sessions::LaunchReport>,
    worker: Option<DisplayWorker>,
    operation: Option<PendingProfileOperation>,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    let activity = crate::profile_activity::ProfileActivity::begin(index);
    cleanup.enqueue(
        lifecycle::Key::Session(index, display_slot),
        LaunchCleanup {
            id: index,
            report,
            worker,
            operation,
            _activity: activity,
        },
        cleanup_launch_resources,
        complete_launch_cleanup,
    );
}

pub(super) fn cleanup_owned_container_session(
    identity: &container_sessions::OwnedContainerIdentity,
) -> Result<(), String> {
    let result = container_sessions::cleanup_owned_instance(identity);
    if let Err(error) = &result {
        log::warn!(
            "Container session '{}' ({}) owned cleanup failed: {}",
            identity.name(),
            identity.runtime(),
            error
        );
    }
    result
}

pub(super) fn finish_reaped_container_session(
    index: ProfileId,
    process: &str,
    status: &str,
    requested_stop: bool,
    had_display: bool,
    cleanup: Result<(), String>,
) {
    if let Err(error) = cleanup {
        container_mode::record_session_log(index, "cleanup", &error);
        container_mode::record_stop_failure(
            index,
            &format!("Owned resource cleanup failed; resources remain reserved for retry: {error}"),
        );
        return;
    }
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

pub(super) fn reap_exited_container_sessions(
    active: &mut Vec<ActiveContainerSession>,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    let mut position = 0;
    while position < active.len() {
        let session = &mut active[position];
        let mut exited = None;
        if let Some(worker) = session.display_worker.as_mut() {
            exited = observed_exit(&mut worker.child, "dedicated display");
        }
        if exited.is_none()
            && let Some(child) = session.container_child.as_mut()
        {
            exited = observed_exit(child, "container");
        }
        if exited.is_none() {
            exited = observed_exit(&mut session.waypipe_child, "waypipe");
        }
        if let Some((process, status)) = exited {
            let session = active.remove(position);
            queue_session_cleanup(session, process, status, cleanup);
            continue;
        }
        if session
            .stopping_since
            .is_some_and(|started| started.elapsed() >= std::time::Duration::from_secs(4))
            && !session.force_stop_offered
        {
            session.force_stop_offered = true;
            container_mode::record_force_stop_available(session.profile_id);
        }
        position += 1;
    }
}

pub(super) fn observed_exit(
    child: &mut std::process::Child,
    process: &'static str,
) -> Option<(&'static str, String)> {
    match child.try_wait() {
        Ok(Some(status)) => Some((process, status.to_string())),
        Ok(None) => None,
        Err(error) => Some((process, format!("monitor error: {error}"))),
    }
}

pub(super) fn reap_exited_managed_displays(
    displays: &mut Vec<ManagedDisplay>,
    active: &mut Vec<ActiveContainerSession>,
    cleanup: &mut lifecycle::CleanupQueue,
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
                    .map(|session| session.profile_id)
                    .collect::<Vec<_>>();
                for index in affected {
                    let _ = stop_active_container_session(active, index, cleanup);
                }
                enqueue_display_cleanup(
                    DisplayCleanup {
                        worker: display.worker,
                        slot: display.slot,
                        exit_reason: Some(status.to_string()),
                        failed_launch: None,
                        _reservation: None,
                        _activity: None,
                    },
                    cleanup,
                );
            }
            Ok(None) => position += 1,
            Err(error) => {
                let display = displays.remove(position);
                let affected = active
                    .iter()
                    .filter(|session| session.display_slot == display.slot)
                    .map(|session| session.profile_id)
                    .collect::<Vec<_>>();
                for id in affected {
                    let _ = stop_active_container_session(active, id, cleanup);
                }
                enqueue_display_cleanup(
                    DisplayCleanup {
                        worker: display.worker,
                        slot: display.slot,
                        exit_reason: Some(format!("monitor error: {error}")),
                        failed_launch: None,
                        _reservation: None,
                        _activity: None,
                    },
                    cleanup,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for rollback ownership release"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn disconnected_display_releases_producer_only_after_successful_cleanup() {
        let producers = crate::lifecycle_producers::Producers::default();
        let producer = producers.begin().unwrap();
        producers.close();
        let directory = tempfile::tempdir().unwrap();
        let runtime_dir = directory.path().join("runtime");
        std::fs::create_dir(&runtime_dir).unwrap();
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        rollback_disconnected_display(
            DisplayWorker {
                child,
                runtime_dir: runtime_dir.clone(),
            },
            producer,
        );
        assert_eq!(producers.active(), 0);
        assert!(!runtime_dir.exists());
    }

    #[test]
    fn disconnected_display_failure_retains_producer_after_timeout_and_reaper_handoff() {
        let producers = crate::lifecycle_producers::Producers::default();
        let producer = producers.begin().unwrap();
        producers.close();
        let directory = tempfile::tempdir().unwrap();
        let runtime_dir = directory.path().join("runtime");
        std::fs::write(&runtime_dir, b"force directory cleanup failure").unwrap();
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let started = Instant::now();
        rollback_disconnected_owned(
            DisplayWorker {
                child,
                runtime_dir: runtime_dir.clone(),
            },
            producer,
            "display",
            terminate_display_worker,
            Duration::from_millis(20),
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(producers.active(), 1);
        assert!(runtime_dir.is_file());
        std::fs::remove_file(&runtime_dir).unwrap();
        std::fs::create_dir(&runtime_dir).unwrap();
        wait_until(|| producers.active() == 0);
        assert!(!runtime_dir.exists());
    }

    #[test]
    fn disconnected_rollback_panic_keeps_resources_and_producer_until_retry_succeeds() {
        struct Resource {
            attempts: Arc<AtomicUsize>,
            allowed: Arc<std::sync::atomic::AtomicBool>,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Resource {
            fn drop(&mut self) {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
        let producers = crate::lifecycle_producers::Producers::default();
        let producer = producers.begin().unwrap();
        producers.close();
        let attempts = Arc::new(AtomicUsize::new(0));
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dropped = Arc::new(AtomicUsize::new(0));
        rollback_disconnected_owned(
            Resource {
                attempts: Arc::clone(&attempts),
                allowed: Arc::clone(&allowed),
                dropped: Arc::clone(&dropped),
            },
            producer,
            "panic fixture",
            |resource| {
                if resource.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("injected disconnected rollback panic");
                }
                if resource.allowed.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err("rollback still blocked".into())
                }
            },
            Duration::from_millis(20),
        );
        assert!(attempts.load(Ordering::SeqCst) >= 1);
        assert_eq!(producers.active(), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        allowed.store(true, Ordering::SeqCst);
        wait_until(|| producers.active() == 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanup_reports_all_resource_failures() {
        let mut errors = Vec::new();
        collect_error(&mut errors, "Waypipe", Err("kill failed".into()));
        collect_error(&mut errors, "container", Ok(()));
        collect_error(&mut errors, "display", Err("directory busy".into()));
        assert_eq!(
            cleanup_result(errors),
            Err("Waypipe: kill failed; display: directory busy".into())
        );
    }

    #[test]
    fn display_cleanup_is_retry_safe_after_exit_and_directory_removal() {
        let directory = tempfile::tempdir().unwrap();
        let runtime_dir = directory.path().join("display-runtime");
        std::fs::create_dir(&runtime_dir).unwrap();
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let mut worker = DisplayWorker { child, runtime_dir };
        assert!(terminate_display_worker(&mut worker).is_ok());
        assert!(terminate_display_worker(&mut worker).is_ok());
        assert!(!worker.runtime_dir.exists());
    }

    #[test]
    fn runtime_directory_cleanup_rejects_empty_paths_and_accepts_missing_owned_path() {
        assert!(remove_runtime_directory(&mut std::path::PathBuf::new()).is_err());
        let directory = tempfile::tempdir().unwrap();
        let mut path = directory.path().join("runtime");
        std::fs::create_dir(&path).unwrap();
        assert!(remove_runtime_directory(&mut path).is_ok());
        assert!(remove_runtime_directory(&mut path).is_ok());
    }

    #[test]
    fn classic_child_cleanup_reaps_without_waiting_on_the_caller() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut cleanup = lifecycle::CleanupQueue::new(|| {});
        let started = Instant::now();
        queue_classic_child(child, &mut cleanup);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(cleanup.finish(Duration::from_secs(3)));
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn cleanup_failure_holds_real_identity_and_profile_leases_until_retry_succeeds() {
        struct Resources {
            worker: DisplayWorker,
            _reservation: crate::profile_activity::RuntimeReservation,
            _activity: crate::profile_activity::ProfileActivity,
            failures: Arc<AtomicUsize>,
        }
        let id = ProfileId::new().unwrap();
        let other_id = ProfileId::new().unwrap();
        let name = format!("lifecycle-retry-{id}");
        let activity = crate::profile_activity::ProfileActivity::begin(id);
        let reservation =
            crate::profile_activity::reserve_identity(id, 1, "container", &name).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let runtime_dir = directory.path().join("display-runtime");
        // A regular file forces remove_dir_all to fail after the child is reaped.
        std::fs::write(&runtime_dir, b"not a directory").unwrap();
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let failures = Arc::new(AtomicUsize::new(0));
        let mut cleanup = lifecycle::CleanupQueue::new(|| {});
        cleanup.enqueue(
            lifecycle::Key::Session(id, "retry-display".into()),
            Resources {
                worker: DisplayWorker {
                    child,
                    runtime_dir: runtime_dir.clone(),
                },
                _reservation: reservation,
                _activity: activity,
                failures: Arc::clone(&failures),
            },
            |resources| terminate_display_worker(&mut resources.worker),
            |resources, outcome| {
                if outcome.is_err() {
                    resources.failures.fetch_add(1, Ordering::SeqCst);
                }
            },
        );
        assert!(!cleanup.finish(Duration::from_millis(30)));
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert!(cleanup.session_busy(id));
        assert!(
            crate::profile_activity::reserve_identity(other_id, 2, "container", &name).is_err()
        );
        assert!(crate::profile_activity::while_inactive(id, || Ok(())).is_err());
        std::fs::remove_file(&runtime_dir).unwrap();
        std::fs::create_dir(&runtime_dir).unwrap();
        assert!(cleanup.finish(Duration::from_secs(3)));
        let deadline = Instant::now() + Duration::from_secs(3);
        while crate::profile_activity::identity_reserved_by_other("container", &name, None) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(crate::profile_activity::reserve_identity(other_id, 3, "container", &name).is_ok());
        assert!(crate::profile_activity::while_inactive(id, || Ok(())).is_ok());
    }
}
