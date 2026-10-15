//! CPU ownership model; Metal command buffers retain the actual GPU resources.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

pub(crate) const MAX_FRAMES: usize = 3;
const MAX_VERSIONS: usize = MAX_FRAMES;
const REDRAW_WAITING: usize = 1 << (usize::BITS - 1);

#[derive(Default)]
pub(crate) struct FrameGate {
    // One atomic word prevents a completion from racing past deferred redraw demand.
    state: AtomicUsize,
}

impl FrameGate {
    pub(crate) fn ready(&self) -> bool {
        self.admit(false)
    }

    pub(crate) fn begin(self: &Arc<Self>) -> Option<Arc<FrameLifetime>> {
        self.admit(true).then(|| {
            Arc::new(FrameLifetime {
                gate: self.clone(),
                completed: AtomicBool::new(false),
            })
        })
    }

    pub(crate) fn defer_redraw(&self) {
        self.state.fetch_or(REDRAW_WAITING, Ordering::AcqRel);
    }

    fn admit(&self, reserve: bool) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            let ready = state & !REDRAW_WAITING < MAX_FRAMES;
            let next = if ready {
                state + usize::from(reserve)
            } else {
                state | REDRAW_WAITING
            };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return ready,
                Err(current) => state = current,
            }
        }
    }

    fn release(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            debug_assert!(state & !REDRAW_WAITING > 0);
            let next = (state - 1) & !REDRAW_WAITING;
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return state & REDRAW_WAITING != 0,
                Err(current) => state = current,
            }
        }
    }
}

/// Held by an unsubmitted frame, then by its completion handler, never just the cache.
pub(crate) struct FrameLifetime {
    gate: Arc<FrameGate>,
    completed: AtomicBool,
}

impl FrameLifetime {
    /// Signal success OR GPU error. Returns whether deferred redraw demand needs waking.
    pub(crate) fn complete(&self) -> bool {
        if self.completed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.gate.release()
    }
}

impl Drop for FrameLifetime {
    fn drop(&mut self) {
        // Also releases frames abandoned before submission.
        self.complete();
    }
}

struct Version<T> {
    resource: T,
    readers: Vec<Weak<FrameLifetime>>,
}

impl<T> Version<T> {
    fn writable(&mut self) -> bool {
        self.readers.retain(|reader| {
            reader
                .upgrade()
                .is_some_and(|frame| !frame.completed.load(Ordering::Acquire))
        });
        self.readers.is_empty()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UpdateError {
    Busy,
    Rejected,
}

pub(crate) struct TextureVersions<T> {
    versions: Vec<Version<T>>,
    current: usize,
}

impl<T> TextureVersions<T> {
    pub(crate) fn new() -> Self {
        Self {
            versions: Vec::new(),
            current: 0,
        }
    }

    pub(crate) fn current(&self) -> Option<&T> {
        self.versions
            .get(self.current)
            .map(|version| &version.resource)
    }

    pub(crate) fn mark_read(&mut self, frame: &Arc<FrameLifetime>) {
        let version = &mut self.versions[self.current];
        version.writable();
        let reader = Arc::downgrade(frame);
        if !version
            .readers
            .iter()
            .any(|existing| existing.ptr_eq(&reader))
        {
            version.readers.push(reader);
        }
    }

    /// The callback can CPU-write only the idle resource it receives. `is_current`
    /// permits damage-only updates; other slots need a complete snapshot, not just damage.
    /// A failed callback must leave the existing resource unchanged.
    pub(crate) fn update(
        &mut self,
        upload: impl FnOnce(Option<&T>, bool) -> Option<T>,
    ) -> Result<(), UpdateError> {
        let target = if self
            .versions
            .get_mut(self.current)
            .is_some_and(Version::writable)
        {
            self.current
        } else if let Some(index) = self.versions.iter_mut().position(Version::writable) {
            index
        } else if self.versions.len() < MAX_VERSIONS {
            self.versions.len()
        } else {
            return Err(UpdateError::Busy);
        };
        let existing = self.versions.get(target).map(|version| &version.resource);
        let resource = upload(existing, existing.is_some() && target == self.current)
            .ok_or(UpdateError::Rejected)?;
        if let Some(version) = self.versions.get_mut(target) {
            version.resource = resource;
        } else {
            self.versions.push(Version {
                resource,
                readers: Vec::new(),
            });
        }
        self.current = target;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Pixels = Arc<Mutex<[u8; 4]>>;

    fn upload(versions: &mut TextureVersions<Pixels>, pixels: [u8; 4]) -> Result<(), UpdateError> {
        versions.update(|idle, is_current| {
            let texture = idle
                .cloned()
                .unwrap_or_else(|| Arc::new(Mutex::new([0; 4])));
            if is_current {
                // Simulate a one-pixel damage update on the current image.
                texture.lock().unwrap()[0] = pixels[0];
            } else {
                *texture.lock().unwrap() = pixels;
            }
            Some(texture)
        })
    }

    #[test]
    fn delayed_frames_cannot_observe_later_cpu_uploads() {
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        let mut submitted = Vec::new();
        for pattern in 1..=3 {
            let frame = gate.begin().unwrap();
            upload(&mut versions, [pattern; 4]).unwrap();
            versions.mark_read(&frame);
            submitted.push((frame, versions.current().unwrap().clone(), pattern));
        }
        assert!(gate.begin().is_none());
        assert_eq!(upload(&mut versions, [9; 4]), Err(UpdateError::Busy));
        assert_eq!(versions.versions.len(), MAX_VERSIONS);
        for (_, texture, pattern) in &submitted {
            assert_eq!(*texture.lock().unwrap(), [*pattern; 4]);
        }

        assert!(submitted[0].0.complete());
        let frame = gate.begin().unwrap();
        upload(&mut versions, [4; 4]).unwrap();
        versions.mark_read(&frame);
        assert_eq!(*versions.current().unwrap().lock().unwrap(), [4; 4]);
        assert_eq!(*submitted[1].1.lock().unwrap(), [2; 4]);
        assert_eq!(*submitted[2].1.lock().unwrap(), [3; 4]);
        assert_eq!(versions.versions.len(), MAX_VERSIONS);
    }

    #[test]
    fn cached_draws_keep_a_version_busy_until_all_readers_complete() {
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        upload(&mut versions, [1; 4]).unwrap();
        let first = gate.begin().unwrap();
        let second = gate.begin().unwrap();
        versions.mark_read(&first);
        versions.mark_read(&first);
        versions.mark_read(&second);
        assert_eq!(versions.versions[0].readers.len(), 2);
        first.complete();
        assert!(!versions.versions[0].writable());
        upload(&mut versions, [2; 4]).unwrap();
        assert_eq!(versions.versions.len(), 2);
        second.complete();
        assert!(versions.versions[0].writable());
    }

    #[test]
    fn idle_current_allows_damage_but_rotating_to_stale_slot_requires_full_upload() {
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        upload(&mut versions, [1, 2, 3, 4]).unwrap();
        upload(&mut versions, [9, 0, 0, 0]).unwrap();
        assert_eq!(*versions.current().unwrap().lock().unwrap(), [9, 2, 3, 4]);
        let first = gate.begin().unwrap();
        versions.mark_read(&first);
        upload(&mut versions, [5, 6, 7, 8]).unwrap();
        let second = gate.begin().unwrap();
        versions.mark_read(&second);
        first.complete();
        upload(&mut versions, [10, 11, 12, 13]).unwrap();
        assert_eq!(
            *versions.current().unwrap().lock().unwrap(),
            [10, 11, 12, 13]
        );
    }

    #[test]
    fn abandoned_frame_releases_ownership_without_gpu_completion() {
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        upload(&mut versions, [1; 4]).unwrap();
        let frame = gate.begin().unwrap();
        versions.mark_read(&frame);
        drop(frame);
        upload(&mut versions, [2; 4]).unwrap();
        assert_eq!(versions.versions.len(), 1);
        assert!(gate.begin().is_some());
    }

    #[test]
    fn completion_is_idempotent_and_only_wakes_deferred_demand() {
        let gate = Arc::new(FrameGate::default());
        let first = gate.begin().unwrap();
        let second = gate.begin().unwrap();
        let third = gate.begin().unwrap();
        assert!(!gate.ready());
        assert!(first.complete());
        assert!(!first.complete());
        let fourth = gate.begin().unwrap();
        assert!(!second.complete());
        assert!(gate.ready());
        assert!(!third.complete());
        assert!(!fourth.complete());
        assert_eq!(gate.state.load(Ordering::Acquire), 0);
    }

    #[test]
    fn slot_exhaustion_can_defer_redraw_below_the_frame_limit() {
        let gate = Arc::new(FrameGate::default());
        let frame = gate.begin().unwrap();
        assert!(gate.ready());
        gate.defer_redraw();
        assert!(frame.complete());
        assert!(!frame.complete());
    }

    #[test]
    fn redraw_demand_racing_completion_cannot_lose_its_wakeup() {
        use std::sync::Barrier;

        for _ in 0..100 {
            let gate = Arc::new(FrameGate::default());
            let _first = gate.begin().unwrap();
            let _second = gate.begin().unwrap();
            let third = gate.begin().unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let other_barrier = barrier.clone();
            let completed = std::thread::spawn(move || {
                other_barrier.wait();
                third.complete()
            });
            barrier.wait();
            let ready = gate.ready();
            let woke = completed.join().unwrap();
            assert!(ready || woke, "deferred redraw needs a completion wakeup");
        }
    }

    #[test]
    fn completion_can_arrive_off_thread_and_survive_cache_eviction() {
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        upload(&mut versions, [1; 4]).unwrap();
        let frame = gate.begin().unwrap();
        versions.mark_read(&frame);
        drop(versions);
        assert_eq!(gate.state.load(Ordering::Acquire), 1);
        std::thread::spawn(move || frame.complete()).join().unwrap();
        assert_eq!(gate.state.load(Ordering::Acquire), 0);
    }

    #[test]
    fn failed_upload_does_not_publish_a_new_current_version() {
        let mut versions = TextureVersions::new();
        upload(&mut versions, [1; 4]).unwrap();
        assert_eq!(versions.update(|_, _| None), Err(UpdateError::Rejected));
        assert_eq!(*versions.current().unwrap().lock().unwrap(), [1; 4]);
        assert_eq!(versions.versions.len(), 1);
    }
}
