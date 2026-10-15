//! Drains late resource-bearing worker replies after input admission has closed.
//! A stopped native loop is not proof that its launch producers have stopped.
use std::collections::HashMap;

use super::{DisplayWorker, PendingProfileOperation};
use crate::container_sessions::ProfileId;
use crate::event_loop_signal::{Receiver, Sender};
use crate::messages::CompositorMessage;
use crate::{lifecycle, lifecycle_producers, session_lifecycle};

pub fn drain_owned_results(
    receiver: &Receiver<CompositorMessage>,
    signal: &Sender<CompositorMessage>,
    operations: &mut HashMap<ProfileId, PendingProfileOperation>,
    workers: &mut HashMap<ProfileId, DisplayWorker>,
    cleanup: &mut lifecycle::CleanupQueue,
) -> usize {
    let mut count = 0;
    while count < 256 {
        let Ok(message) = receiver.try_recv() else {
            break;
        };
        signal.message_received();
        count += 1;
        match message {
            CompositorMessage::DedicatedDisplayStarted {
                profile_id,
                generation,
                display_slot,
                worker_child,
                worker_runtime_dir,
                ..
            } => {
                let operation = take_attempt(operations, profile_id, generation);
                session_lifecycle::queue_launch_cleanup(
                    profile_id,
                    display_slot,
                    None,
                    Some(DisplayWorker {
                        child: worker_child,
                        runtime_dir: worker_runtime_dir,
                    }),
                    operation,
                    cleanup,
                );
            }
            CompositorMessage::ManagedDisplayStarted {
                display_slot,
                worker_child,
                worker_runtime_dir,
                ..
            } => session_lifecycle::queue_display_cleanup(
                DisplayWorker {
                    child: worker_child,
                    runtime_dir: worker_runtime_dir,
                },
                display_slot,
                false,
                cleanup,
            ),
            CompositorMessage::ContainerSessionLaunchFinished {
                profile_id,
                generation,
                display_slot,
                result,
            } => {
                let operation = take_attempt(operations, profile_id, generation);
                let worker = operation.as_ref().and_then(|_| workers.remove(&profile_id));
                session_lifecycle::queue_launch_cleanup(
                    profile_id,
                    display_slot,
                    result.ok(),
                    worker,
                    operation,
                    cleanup,
                );
            }
            // Other messages own only bounded data, not processes. Do not replay
            // UI actions (especially queued Launch) during shutdown.
            _ => {}
        }
    }
    count
}

fn take_attempt(
    operations: &mut HashMap<ProfileId, PendingProfileOperation>,
    id: ProfileId,
    generation: u64,
) -> Option<PendingProfileOperation> {
    if super::current_operation(operations, id, generation) {
        operations.remove(&id)
    } else {
        None
    }
}

struct Remainders {
    _operations: HashMap<ProfileId, PendingProfileOperation>,
    workers: HashMap<ProfileId, DisplayWorker>,
}

pub fn retain_remainders(
    operations: HashMap<ProfileId, PendingProfileOperation>,
    workers: HashMap<ProfileId, DisplayWorker>,
    cleanup: &mut lifecycle::CleanupQueue,
) {
    cleanup.enqueue(
        lifecycle::Key::Abandoned,
        Remainders {
            _operations: operations,
            workers,
        },
        |resources| {
            if lifecycle_producers::GLOBAL.active() != 0 {
                return Err(
                    "Launch workers are still rolling back; retaining their identity leases".into(),
                );
            }
            let mut errors = Vec::new();
            for worker in resources.workers.values_mut() {
                if let Err(error) = session_lifecycle::terminate_display_worker(worker) {
                    errors.push(error);
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        },
        |_, result| {
            if let Err(error) = result {
                log::warn!("Shutdown is incomplete: {error}");
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_activity::{ProfileActivity, while_inactive};
    use std::process::Command;
    use std::time::Duration;

    fn attempt(id: ProfileId, generation: u64) -> PendingProfileOperation {
        PendingProfileOperation {
            generation,
            session: toml::from_str(&format!(
                "id = '{id}'\nname = 'Shutdown fixture'\nimage = 'unused'\n"
            ))
            .unwrap(),
            launch_requested: true,
            cancelled: true,
            _profile_activity: ProfileActivity::begin(id),
            identity_reservation: None,
        }
    }

    #[test]
    fn late_display_reply_is_reclaimed_and_its_attempt_lease_survives_until_completion() {
        let id = ProfileId::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let runtime_dir = directory.path().join("worker");
        std::fs::create_dir(&runtime_dir).unwrap();
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let (signal, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        signal
            .send(CompositorMessage::DedicatedDisplayStarted {
                profile_id: id,
                generation: 7,
                display_slot: "shutdown-test".into(),
                runtime_dir: runtime_dir.to_string_lossy().into_owned(),
                display: "wayland-1".into(),
                worker_child: child,
                worker_runtime_dir: runtime_dir.clone(),
            })
            .unwrap_or_else(|_| panic!("fixture delivery failed"));
        let mut operations = HashMap::from([(id, attempt(id, 7))]);
        let mut cleanup = lifecycle::CleanupQueue::new(|| {});
        assert_eq!(
            drain_owned_results(
                &receiver,
                &signal,
                &mut operations,
                &mut HashMap::new(),
                &mut cleanup
            ),
            1
        );
        assert!(operations.is_empty());
        assert!(cleanup.session_busy(id));
        assert!(while_inactive(id, || Ok(())).is_err());
        assert!(cleanup.finish(Duration::from_secs(3)));
        assert!(!runtime_dir.exists());
        assert!(while_inactive(id, || Ok(())).is_ok());
    }

    #[test]
    fn old_generation_cannot_take_a_new_attempt_during_shutdown() {
        let id = ProfileId::new().unwrap();
        let mut operations = HashMap::from([(id, attempt(id, 9))]);
        assert!(take_attempt(&mut operations, id, 8).is_none());
        assert_eq!(operations[&id].generation, 9);
        assert!(while_inactive(id, || Ok(())).is_err());
        drop(operations);
        assert!(while_inactive(id, || Ok(())).is_ok());
    }

    #[test]
    fn shutdown_does_not_replay_queued_launch_actions() {
        let id = ProfileId::new().unwrap();
        let (signal, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        signal
            .send(CompositorMessage::StartContainerSession(id))
            .unwrap_or_else(|_| panic!("fixture delivery failed"));
        let mut cleanup = lifecycle::CleanupQueue::new(|| {});
        assert_eq!(
            drain_owned_results(
                &receiver,
                &signal,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut cleanup
            ),
            1
        );
        assert!(cleanup.is_empty());
        assert!(!signal.finish_turn());
    }
}
