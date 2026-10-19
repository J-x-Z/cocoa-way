//! CPU ownership model; Metal command buffers retain the actual GPU resources.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

#[path = "render_budget.rs"]
mod render_budget;
pub(crate) use render_budget::{BudgetError, BudgetWake, TextureBudget, TextureReservation};

pub(crate) const MAX_FRAMES: usize = 3;
const MAX_VERSIONS: usize = MAX_FRAMES;
const REDRAW_WAITING: usize = 1 << (usize::BITS - 1);
const DRAINING: usize = REDRAW_WAITING >> 1;
const FRAME_COUNT: usize = !(REDRAW_WAITING | DRAINING);

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
                completion_started: AtomicBool::new(false),
                completed: AtomicBool::new(false),
                reservations: Mutex::new(Vec::new()),
            })
        })
    }

    pub(crate) fn defer_redraw(&self) {
        self.state.fetch_or(REDRAW_WAITING, Ordering::AcqRel);
    }

    /// Budget pressure may require reusing the only current texture, rather than
    /// continually adding cached fallback readers while waiting for it to idle.
    pub(crate) fn defer_until_idle(&self) {
        self.state
            .fetch_or(DRAINING | REDRAW_WAITING, Ordering::AcqRel);
    }

    fn admit(&self, reserve: bool) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            let count = state & FRAME_COUNT;
            let ready = count < MAX_FRAMES && (state & DRAINING == 0 || count == 0);
            let next = if ready {
                (state & !DRAINING) + usize::from(reserve)
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
            debug_assert!(state & FRAME_COUNT > 0);
            let draining = state & DRAINING != 0 && (state & FRAME_COUNT) > 1;
            let next = if draining {
                state - 1
            } else {
                (state - 1) & !REDRAW_WAITING
            };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return !draining && state & REDRAW_WAITING != 0,
                Err(current) => state = current,
            }
        }
    }
}

/// Held by an unsubmitted frame, then by its completion handler, never just the cache.
pub(crate) struct FrameLifetime {
    gate: Arc<FrameGate>,
    completion_started: AtomicBool,
    completed: AtomicBool,
    reservations: Mutex<Vec<Arc<TextureReservation>>>,
}

impl FrameLifetime {
    pub(crate) fn accepts_preflight(self: &Arc<Self>, prepared: &Weak<Self>) -> bool {
        prepared.ptr_eq(&Arc::downgrade(self)) && !self.completion_started.load(Ordering::Acquire)
    }

    /// Signal success OR GPU error. Wake deferred demand or reclaim retired cache slots.
    pub(crate) fn complete(&self) -> bool {
        if self.completion_started.swap(true, Ordering::AcqRel) {
            return false;
        }
        let reservations = {
            let mut reservations = self.reservations.lock().unwrap();
            for reservation in reservations.iter() {
                reservation.end_read();
            }
            std::mem::take(&mut *reservations)
        };
        // A writable version must also have finished its reader accounting, so
        // preflight can safely credit an idle replacement against the local share.
        self.completed.store(true, Ordering::Release);
        // Publish completion BEFORE checking retirement: if retirement races us,
        // either its pruning sees an idle slot or this check requests maintenance.
        let reclaim = reservations
            .iter()
            .any(|reservation| reservation.is_retired());
        drop(reservations); // Budget callbacks run outside the frame's reader lock.
        self.gate.release() || reclaim
    }

    pub(crate) fn retain_reservation(&self, reservation: &Arc<TextureReservation>) {
        let mut reservations = self.reservations.lock().unwrap();
        if !self.completion_started.load(Ordering::Acquire)
            && !reservations
                .iter()
                .any(|existing| Arc::ptr_eq(existing, reservation))
        {
            reservation.begin_read();
            reservations.push(reservation.clone());
        }
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrepareError<E> {
    Busy,
    Allocation(E),
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

    pub(crate) fn current_busy(&mut self) -> bool {
        self.versions
            .get_mut(self.current)
            .is_some_and(|version| !version.writable())
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

    /// The current image is the only surviving copy after wl_buffer.release.
    /// Under pressure reclaim every idle alternate, never the current image or a reader.
    pub(crate) fn prune_idle(&mut self) -> usize {
        self.retain_idle_spare(|_| false)
    }

    pub(crate) fn has_alternates(&self) -> bool {
        self.versions.len() > 1
    }

    /// Keep one reusable, still budget-accounted alternate. Busy versions do not
    /// compete for this spare; they must survive until ALL their readers complete.
    pub(crate) fn retain_idle_spare(&mut self, compatible: impl Fn(&T) -> bool) -> usize {
        let previous_len = self.versions.len();
        let mut index = 0;
        let mut current = self.current;
        let mut spare = false;
        self.versions.retain_mut(|version| {
            let keep = if index == self.current || !version.writable() {
                true
            } else if !spare && compatible(&version.resource) {
                spare = true;
                true
            } else {
                false
            };
            if !keep && index < self.current {
                current -= 1;
            }
            index += 1;
            keep
        });
        self.current = current;
        previous_len - self.versions.len()
    }

    /// The callback can CPU-write only the idle resource it receives. `is_current`
    /// permits damage-only updates; other slots need a complete snapshot, not just damage.
    /// A failed callback must leave the existing resource unchanged.
    #[allow(dead_code)] // Also used by the non-test scripts/audit-shm.rs rustc entry point.
    pub(crate) fn update(
        &mut self,
        upload: impl FnOnce(Option<&T>, bool) -> Option<T>,
    ) -> Result<(), UpdateError> {
        self.update_matching(|_| true, upload)
    }

    fn update_target(&mut self, compatible: impl Fn(&T) -> bool) -> Result<usize, UpdateError> {
        // Prefer compatible storage even when the idle current image has a
        // different size. Otherwise a resize could allocate despite a usable spare.
        if self
            .versions
            .get_mut(self.current)
            .is_some_and(|version| version.writable() && compatible(&version.resource))
        {
            return Ok(self.current);
        }
        if let Some(index) = self
            .versions
            .iter_mut()
            .position(|version| version.writable() && compatible(&version.resource))
        {
            return Ok(index);
        }
        if self
            .versions
            .get_mut(self.current)
            .is_some_and(Version::writable)
        {
            return Ok(self.current);
        }
        if let Some(index) = self.versions.iter_mut().position(Version::writable) {
            return Ok(index);
        }
        if self.versions.len() < MAX_VERSIONS {
            Ok(self.versions.len())
        } else {
            Err(UpdateError::Busy)
        }
    }

    /// Metadata-only admission. Completion can only make more versions writable;
    /// callers must not mutate the cache between this check and `update_matching`.
    pub(crate) fn writable_matching(
        &mut self,
        compatible: impl Fn(&T) -> bool,
    ) -> Result<Option<&T>, UpdateError> {
        let target = self.update_target(compatible)?;
        Ok(self.versions.get(target).map(|version| &version.resource))
    }

    /// Allocate only when no compatible idle version exists. The returned storage
    /// stays owned by the caller until the snapshot succeeds; publishing must NOT
    /// allocate again. Without cache mutation, completion can only add idle choices.
    pub(crate) fn prepare_matching<E>(
        &mut self,
        compatible: impl Fn(&T) -> bool,
        allocate: impl FnOnce(Option<&T>) -> Result<T, E>,
    ) -> Result<Option<T>, PrepareError<E>> {
        let idle = self
            .writable_matching(&compatible)
            .map_err(|_| PrepareError::Busy)?;
        if idle.is_some_and(compatible) {
            Ok(None)
        } else {
            allocate(idle).map(Some).map_err(PrepareError::Allocation)
        }
    }

    pub(crate) fn update_matching(
        &mut self,
        compatible: impl Fn(&T) -> bool,
        upload: impl FnOnce(Option<&T>, bool) -> Option<T>,
    ) -> Result<(), UpdateError> {
        let target = self.update_target(compatible)?;
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

    #[test]
    fn budget_survives_cache_eviction_until_last_gpu_reader_completes() {
        let budget = TextureBudget::new(8, 2);
        let wakeups = Arc::new(AtomicUsize::new(0));
        let callback_wakeups = wakeups.clone();
        let wake = BudgetWake::new(move || {
            callback_wakeups.fetch_add(1, Ordering::Relaxed);
        });
        let reservation = budget.reserve(4, &wake).unwrap();
        let weak = Arc::downgrade(&reservation);
        let gate = Arc::new(FrameGate::default());
        let first = gate.begin().unwrap();
        let second = gate.begin().unwrap();
        first.retain_reservation(&reservation);
        first.retain_reservation(&reservation);
        second.retain_reservation(&reservation);
        assert_eq!(first.reservations.lock().unwrap().len(), 1);
        drop(reservation); // Simulate evicting/dropping the renderer cache.
        assert!(weak.upgrade().is_some());
        assert!(budget.reserve(8, &wake).is_none());
        first.complete();
        assert!(weak.upgrade().is_some());
        assert!(budget.reserve(8, &wake).is_none());
        assert_eq!(wakeups.load(Ordering::Relaxed), 0);
        std::thread::spawn(move || second.complete())
            .join()
            .unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(wakeups.load(Ordering::Relaxed), 1);
        assert!(budget.reserve(8, &wake).is_some());
        assert!(!first.complete());
    }

    #[test]
    fn pruning_reclaims_only_idle_alternates_and_preserves_current_image() {
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        upload(&mut versions, [1; 4]).unwrap();
        let first = gate.begin().unwrap();
        versions.mark_read(&first);
        upload(&mut versions, [2; 4]).unwrap();
        versions.prune_idle();
        assert_eq!(versions.versions.len(), 2);
        first.complete();
        versions.prune_idle();
        assert_eq!(versions.versions.len(), 1);
        assert_eq!(*versions.current().unwrap().lock().unwrap(), [2; 4]);
        // An idle current slot remains the only complete copy of released SHM.
        versions.prune_idle();
        assert_eq!(versions.versions.len(), 1);
    }

    #[test]
    fn retired_completion_requests_reclamation_without_polling_current_images() {
        let budget = TextureBudget::shared();
        let wake = BudgetWake::new(|| {});
        let reservation = budget.reserve(4, &wake).unwrap();
        let gate = Arc::new(FrameGate::default());
        let frame = gate.begin().unwrap();
        frame.retain_reservation(&reservation);
        reservation.set_retired(true);
        assert!(frame.complete());
        assert!(!frame.complete());
        reservation.set_retired(false); // Reuse an idle version as the current image.
        let frame = gate.begin().unwrap();
        frame.retain_reservation(&reservation);
        assert!(!frame.complete()); // A cached fallback cannot start a redraw loop.
    }

    #[test]
    fn budget_pressure_drains_fallback_readers_before_retrying_in_place() {
        let gate = Arc::new(FrameGate::default());
        let first = gate.begin().unwrap();
        let fallback = gate.begin().unwrap();
        gate.defer_until_idle();
        assert!(!gate.ready());
        assert!(gate.begin().is_none());
        assert!(!first.complete());
        assert!(!gate.ready());
        assert!(fallback.complete());
        assert!(gate.ready());
        let retry = gate.begin().unwrap();
        assert!(!retry.complete());
        assert_eq!(gate.state.load(Ordering::Acquire), 0);
    }

    #[derive(Clone)]
    struct AccountedPixels {
        pixels: Pixels,
        reservation: Arc<TextureReservation>,
    }

    fn accounted_upload(
        versions: &mut TextureVersions<AccountedPixels>,
        budget: &Arc<TextureBudget>,
        wake: &Arc<BudgetWake>,
        bytes: usize,
        pattern: u8,
        allocations: &mut usize,
    ) -> Result<(), UpdateError> {
        versions.retain_idle_spare(|resource| resource.reservation.bytes == bytes);
        let previous = versions
            .current()
            .map(|resource| resource.reservation.clone());
        versions.update_matching(
            |resource| resource.reservation.bytes == bytes,
            |idle, _| {
                let resource = match idle.filter(|resource| resource.reservation.bytes == bytes) {
                    Some(resource) => resource.clone(),
                    None => {
                        let reservation = budget
                            .try_reserve_replacing(
                                bytes,
                                wake,
                                idle.map(|resource| &resource.reservation),
                            )
                            .ok()?;
                        *allocations += 1;
                        AccountedPixels {
                            pixels: Arc::new(Mutex::new([0; 4])),
                            reservation,
                        }
                    }
                };
                *resource.pixels.lock().unwrap() = [pattern; 4];
                Some(resource)
            },
        )?;
        let current = &versions.current().unwrap().reservation;
        current.set_retired(false);
        if let Some(previous) = previous
            && !Arc::ptr_eq(&previous, current)
        {
            previous.set_retired(true);
        }
        versions.retain_idle_spare(|resource| resource.reservation.bytes == bytes);
        Ok(())
    }

    fn read_accounted(versions: &mut TextureVersions<AccountedPixels>, frame: &Arc<FrameLifetime>) {
        versions.mark_read(frame);
        frame.retain_reservation(&versions.current().unwrap().reservation);
    }

    #[test]
    fn overlapping_frames_reuse_one_accounted_spare_without_redundant_allocations() {
        let budget = TextureBudget::new(12, 3);
        let wake = BudgetWake::new(|| {});
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        let mut allocations = 0;
        accounted_upload(&mut versions, &budget, &wake, 4, 0, &mut allocations).unwrap();
        let mut previous_frame = gate.begin().unwrap();
        read_accounted(&mut versions, &previous_frame);
        for pattern in 1..=200 {
            // Model maintenance at frame start AND before/after every upload.
            versions.retain_idle_spare(|_| true);
            let old_pixels = versions.current().unwrap().pixels.clone();
            let old_pattern = *old_pixels.lock().unwrap();
            let next_frame = gate.begin().unwrap();
            accounted_upload(&mut versions, &budget, &wake, 4, pattern, &mut allocations).unwrap();
            read_accounted(&mut versions, &next_frame);
            assert_eq!(*old_pixels.lock().unwrap(), old_pattern);
            assert_eq!(allocations, 2, "steady overlap must not allocate again");
            previous_frame.complete();
            versions.retain_idle_spare(|_| true);
            assert_eq!(versions.versions.len(), 2);
            previous_frame = next_frame;
        }
        previous_frame.complete();
        // Both retained textures remain charged: there is exactly one slot/4 bytes free.
        let remaining = budget.reserve(4, &wake).unwrap();
        assert!(budget.reserve(1, &wake).is_none());
        drop(remaining);
        drop(versions);
        assert!(budget.reserve(12, &wake).is_some());
    }

    #[test]
    fn spare_trimming_preserves_current_and_readers_in_every_completion_order() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let budget = TextureBudget::new(12, 3);
            let wake = BudgetWake::new(|| {});
            let gate = Arc::new(FrameGate::default());
            let mut versions = TextureVersions::new();
            let mut allocations = 0;
            let mut frames = Vec::new();
            for pattern in 1..=3 {
                let frame = gate.begin().unwrap();
                accounted_upload(&mut versions, &budget, &wake, 4, pattern, &mut allocations)
                    .unwrap();
                read_accounted(&mut versions, &frame);
                frames.push((frame, versions.current().unwrap().pixels.clone(), pattern));
            }
            for completed in order {
                frames[completed].0.complete();
                versions.retain_idle_spare(|_| true);
                assert_eq!(*versions.current().unwrap().pixels.lock().unwrap(), [3; 4]);
                let current = versions.current;
                let idle_alternates = versions
                    .versions
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(index, version)| {
                        (index != current && version.writable()).then_some(())
                    })
                    .count();
                assert!(idle_alternates <= 1);
                for (frame, pixels, pattern) in &frames {
                    if !frame.completed.load(Ordering::Acquire) {
                        assert_eq!(*pixels.lock().unwrap(), [*pattern; 4]);
                        assert!(
                            versions
                                .versions
                                .iter()
                                .any(|version| { Arc::ptr_eq(&version.resource.pixels, pixels) })
                        );
                    }
                }
            }
            assert_eq!(allocations, 3);
            assert_eq!(versions.versions.len(), 2);
        }
    }

    #[test]
    fn pressure_cannot_reclaim_an_alternate_until_its_last_cached_reader_completes() {
        for order in [[0, 1], [1, 0]] {
            let budget = TextureBudget::new(12, 3);
            let wake = BudgetWake::new(|| {});
            let gate = Arc::new(FrameGate::default());
            let mut versions = TextureVersions::new();
            let mut allocations = 0;
            accounted_upload(&mut versions, &budget, &wake, 4, 1, &mut allocations).unwrap();
            let readers = [gate.begin().unwrap(), gate.begin().unwrap()];
            read_accounted(&mut versions, &readers[0]);
            read_accounted(&mut versions, &readers[1]);
            accounted_upload(&mut versions, &budget, &wake, 4, 2, &mut allocations).unwrap();
            let alternate = Arc::downgrade(&versions.versions[0].resource.reservation);
            assert_eq!(versions.prune_idle(), 0);
            readers[order[0]].complete();
            assert_eq!(versions.prune_idle(), 0);
            assert!(alternate.upgrade().is_some());
            assert!(budget.reserve(8, &wake).is_none());
            readers[order[1]].complete();
            assert_eq!(versions.prune_idle(), 1);
            assert!(alternate.upgrade().is_none());
            assert_eq!(*versions.current().unwrap().pixels.lock().unwrap(), [2; 4]);
            assert!(budget.reserve(8, &wake).is_some());
        }
    }

    #[test]
    fn compatible_spare_beats_idle_wrong_sized_current_without_allocating() {
        let budget = TextureBudget::new(24, 3);
        let wake = BudgetWake::new(|| {});
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        let mut allocations = 0;
        accounted_upload(&mut versions, &budget, &wake, 4, 1, &mut allocations).unwrap();
        let first = gate.begin().unwrap();
        read_accounted(&mut versions, &first);
        accounted_upload(&mut versions, &budget, &wake, 8, 2, &mut allocations).unwrap();
        first.complete();
        // Before maintenance for the old size, the new commit can reuse this slot.
        assert_eq!(
            versions
                .writable_matching(|resource| resource.reservation.bytes == 4)
                .unwrap()
                .unwrap()
                .reservation
                .bytes,
            4
        );
        accounted_upload(&mut versions, &budget, &wake, 4, 3, &mut allocations).unwrap();
        assert_eq!(allocations, 2);
        assert_eq!(versions.versions.len(), 1); // Wrong-sized idle alternate was reclaimed.
        assert_eq!(*versions.current().unwrap().pixels.lock().unwrap(), [3; 4]);
    }

    #[test]
    fn cross_cache_pressure_and_eviction_keep_current_and_in_flight_charges() {
        for (max_bytes, max_allocations) in [(12, 8), (64, 2)] {
            let budget = TextureBudget::new(max_bytes, max_allocations);
            let a = BudgetWake::new(|| {});
            let b = BudgetWake::new(|| {});
            let gate = Arc::new(FrameGate::default());
            let mut versions = TextureVersions::new();
            let mut allocations = 0;
            accounted_upload(&mut versions, &budget, &a, 4, 1, &mut allocations).unwrap();
            let first = gate.begin().unwrap();
            read_accounted(&mut versions, &first);
            accounted_upload(&mut versions, &budget, &a, 4, 2, &mut allocations).unwrap();
            let current = gate.begin().unwrap();
            read_accounted(&mut versions, &current);
            first.complete();
            versions.retain_idle_spare(|_| true);
            let spare = Arc::downgrade(&versions.versions[0].resource.reservation);
            let image = Arc::downgrade(&versions.current().unwrap().reservation);
            assert!(budget.reserve(8, &b).is_none()); // Bytes OR allocation count.
            assert_eq!(versions.prune_idle(), 1);
            assert!(spare.upgrade().is_none());
            let other_cache = budget.reserve(8, &b).unwrap();
            assert!(image.upgrade().is_some());
            assert_eq!(*versions.current().unwrap().pixels.lock().unwrap(), [2; 4]);
            drop(versions); // Cache removal cannot release a GPU reader's charge.
            assert!(image.upgrade().is_some());
            assert!(budget.reserve(4, &b).is_none());
            current.complete();
            assert!(image.upgrade().is_none());
            assert!(budget.reserve(4, &b).is_some());
            drop(other_cache);
        }
    }

    #[test]
    fn preflight_busy_never_enters_upload_and_completed_slot_becomes_reusable() {
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        let mut frames = Vec::new();
        for pattern in 1..=3 {
            upload(&mut versions, [pattern; 4]).unwrap();
            let frame = gate.begin().unwrap();
            versions.mark_read(&frame);
            frames.push(frame);
        }
        assert!(matches!(
            versions.writable_matching(|_| true),
            Err(UpdateError::Busy)
        ));
        assert_eq!(
            versions.update_matching(|_| true, |_, _| panic!("busy upload copied pixels")),
            Err(UpdateError::Busy)
        );
        frames[1].complete(); // Out-of-order completion, not the current image.
        assert!(versions.writable_matching(|_| true).unwrap().is_some());
        upload(&mut versions, [4; 4]).unwrap();
        assert_eq!(versions.versions.len(), 3);
    }

    #[test]
    fn completion_racing_retirement_is_either_prunable_or_wakes_maintenance() {
        use std::sync::Barrier;

        for _ in 0..100 {
            let budget = TextureBudget::new(8, 2);
            let wake = BudgetWake::new(|| {});
            let gate = Arc::new(FrameGate::default());
            let mut versions = TextureVersions::new();
            let mut allocations = 0;
            accounted_upload(&mut versions, &budget, &wake, 4, 1, &mut allocations).unwrap();
            let frame = gate.begin().unwrap();
            read_accounted(&mut versions, &frame);
            let barrier = Arc::new(Barrier::new(2));
            let other_barrier = barrier.clone();
            let completed = std::thread::spawn(move || {
                other_barrier.wait();
                frame.complete()
            });
            barrier.wait();
            accounted_upload(&mut versions, &budget, &wake, 4, 2, &mut allocations).unwrap();
            let removed = versions.prune_idle();
            let woke = completed.join().unwrap();
            // Completion can also allow the upload to update the original slot
            // in place; then there is no alternate requiring reclamation at all.
            assert!(removed != 0 || woke || !versions.has_alternates());
            versions.prune_idle();
            assert!(budget.reserve(4, &wake).is_some());
        }
    }

    #[test]
    fn completing_frames_stay_busy_until_reader_accounting_is_finished() {
        let budget = TextureBudget::new(8, 2);
        let wake = BudgetWake::new(|| {});
        let gate = Arc::new(FrameGate::default());
        let frame = gate.begin().unwrap();
        let mut versions = TextureVersions::new();
        let mut allocations = 0;
        accounted_upload(&mut versions, &budget, &wake, 4, 1, &mut allocations).unwrap();
        read_accounted(&mut versions, &frame);
        let reservation = versions.current().unwrap().reservation.clone();
        let reader_lock = frame.reservations.lock().unwrap();
        let worker_frame = frame.clone();
        let completed = std::thread::spawn(move || worker_frame.complete());
        while !frame.completion_started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        assert!(reservation.has_readers());
        assert!(!frame.completed.load(Ordering::Acquire));
        assert!(versions.current_busy());
        assert!(!frame.complete()); // A concurrent second completion cannot release the gate.
        drop(reader_lock);
        completed.join().unwrap();
        assert!(frame.completed.load(Ordering::Acquire));
        assert!(!reservation.has_readers());
        assert!(!versions.current_busy());
        let late = budget.reserve(4, &wake).unwrap();
        frame.retain_reservation(&late);
        assert!(!late.has_readers()); // No reader can be added to a completing/completed frame.
    }

    #[test]
    fn completion_budget_callbacks_observe_published_completion_without_the_reader_lock() {
        let budget = TextureBudget::new(8, 2);
        let gate = Arc::new(FrameGate::default());
        let frame = gate.begin().unwrap();
        let weak = Arc::downgrade(&frame);
        let callbacks = Arc::new(AtomicUsize::new(0));
        let callback_count = callbacks.clone();
        let wake = BudgetWake::new(move || {
            let frame = weak.upgrade().unwrap();
            assert!(frame.completed.load(Ordering::Acquire));
            assert!(frame.reservations.try_lock().is_ok());
            callback_count.fetch_add(1, Ordering::Relaxed);
        });
        let reservation = budget.reserve(4, &wake).unwrap();
        frame.retain_reservation(&reservation);
        drop(reservation); // Only the frame owns the charge now.
        assert!(budget.reserve(8, &wake).is_none());
        frame.complete();
        assert_eq!(callbacks.load(Ordering::Relaxed), 1);
        assert!(budget.reserve(8, &wake).is_some());
    }

    #[test]
    fn preflight_identity_rejects_other_and_completed_frames() {
        let gate = Arc::new(FrameGate::default());
        let first = gate.begin().unwrap();
        let token = Arc::downgrade(&first);
        let other = gate.begin().unwrap();
        assert!(first.accepts_preflight(&token));
        assert!(!other.accepts_preflight(&token));
        first.complete();
        assert!(!first.accepts_preflight(&token));
        drop(first);
        let next = gate.begin().unwrap();
        assert!(!next.accepts_preflight(&token));
    }

    fn prepare_accounted(
        versions: &mut TextureVersions<AccountedPixels>,
        budget: &Arc<TextureBudget>,
        wake: &Arc<BudgetWake>,
        allocations: &mut usize,
    ) -> Result<Option<AccountedPixels>, PrepareError<BudgetError>> {
        versions.prepare_matching(
            |resource| resource.reservation.bytes == 4,
            |idle| {
                let reservation = budget.try_reserve_replacing(
                    4,
                    wake,
                    idle.map(|resource| &resource.reservation),
                )?;
                *allocations += 1;
                Ok(AccountedPixels {
                    pixels: Arc::new(Mutex::new([0; 4])),
                    reservation,
                })
            },
        )
    }

    fn publish_accounted(
        versions: &mut TextureVersions<AccountedPixels>,
        prepared: Option<AccountedPixels>,
        pattern: u8,
    ) -> Result<(), UpdateError> {
        versions.update_matching(
            |resource| resource.reservation.bytes == 4,
            |idle, _| {
                let resource = idle
                    .filter(|resource| resource.reservation.bytes == 4)
                    .cloned()
                    .or(prepared)?;
                *resource.pixels.lock().unwrap() = [pattern; 4];
                Some(resource)
            },
        )
    }

    #[test]
    fn preflight_reservation_is_consumed_once_without_a_publish_allocation() {
        let budget = TextureBudget::new(8, 2);
        let wake = BudgetWake::new(|| {});
        let mut versions = TextureVersions::new();
        let mut allocations = 0;
        let prepared = prepare_accounted(&mut versions, &budget, &wake, &mut allocations).unwrap();
        let reservation = Arc::downgrade(&prepared.as_ref().unwrap().reservation);
        assert!(budget.reserve(8, &wake).is_none());
        publish_accounted(&mut versions, prepared, 1).unwrap();
        assert_eq!(allocations, 1);
        assert!(reservation.upgrade().is_some());
        assert_eq!(*versions.current().unwrap().pixels.lock().unwrap(), [1; 4]);
        let reused = prepare_accounted(&mut versions, &budget, &wake, &mut allocations).unwrap();
        assert!(reused.is_none());
        publish_accounted(&mut versions, reused, 2).unwrap();
        assert_eq!(allocations, 1);
        drop(versions);
        assert!(reservation.upgrade().is_none());
        assert!(budget.reserve(8, &wake).is_some());
    }

    #[test]
    fn completion_between_preflight_and_publish_drops_the_unused_new_allocation() {
        let budget = TextureBudget::new(8, 2);
        let wake = BudgetWake::new(|| {});
        let gate = Arc::new(FrameGate::default());
        let mut versions = TextureVersions::new();
        let mut allocations = 0;
        accounted_upload(&mut versions, &budget, &wake, 4, 1, &mut allocations).unwrap();
        let frame = gate.begin().unwrap();
        read_accounted(&mut versions, &frame);
        let existing = Arc::downgrade(&versions.current().unwrap().reservation);
        let prepared = prepare_accounted(&mut versions, &budget, &wake, &mut allocations).unwrap();
        let unused = Arc::downgrade(&prepared.as_ref().unwrap().reservation);
        assert!(budget.reserve(1, &wake).is_none());
        frame.complete(); // Publishing may now prefer the compatible current image.
        publish_accounted(&mut versions, prepared, 2).unwrap();
        assert_eq!(allocations, 2);
        assert!(existing.upgrade().is_some());
        assert!(unused.upgrade().is_none());
        assert_eq!(versions.versions.len(), 1);
        assert!(budget.reserve(4, &wake).is_some());
    }

    #[test]
    fn failed_or_abandoned_preflight_does_not_publish_or_leak_storage() {
        let budget = TextureBudget::new(4, 1);
        let wake = BudgetWake::new(|| {});
        let mut versions = TextureVersions::new();
        let mut allocations = 0;
        let held = budget.reserve(4, &wake).unwrap();
        assert!(matches!(
            prepare_accounted(&mut versions, &budget, &wake, &mut allocations),
            Err(PrepareError::Allocation(BudgetError::Pressure))
        ));
        assert_eq!(allocations, 0);
        assert!(versions.current().is_none());
        drop(held);
        let prepared = prepare_accounted(&mut versions, &budget, &wake, &mut allocations).unwrap();
        let reservation = Arc::downgrade(&prepared.as_ref().unwrap().reservation);
        drop(prepared); // Snapshot failed before publishing anything.
        assert!(reservation.upgrade().is_none());
        assert!(versions.current().is_none());
        assert!(budget.reserve(4, &wake).is_some());
    }
}
