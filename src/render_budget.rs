//! Process-wide admission for texture storage, including in-flight retired images.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

const MAX_TEXTURE_BYTES: usize = 256 * 1024 * 1024;
const MAX_TEXTURE_ALLOCATIONS: usize = 4096;
// Base renderer share plus at most one oversized frame. Preserve a bounded base
// cache for root/UI subsurfaces so a valid 6K/8K child can also be displayed.
const MAX_RENDERER_TEXTURE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RENDERER_TEXTURE_ALLOCATIONS: usize = 1024;

pub(crate) struct BudgetWake {
    notify: Box<dyn Fn() + Send + Sync>,
    waiting: AtomicBool,
    bytes: AtomicUsize,
    allocations: AtomicUsize,
    oversized_bytes: AtomicUsize,
    oversized_allocations: AtomicUsize,
}

impl BudgetWake {
    pub(crate) fn new(notify: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            notify: Box::new(notify),
            waiting: AtomicBool::new(false),
            bytes: AtomicUsize::new(0),
            allocations: AtomicUsize::new(0),
            oversized_bytes: AtomicUsize::new(0),
            oversized_allocations: AtomicUsize::new(0),
        })
    }
}

#[derive(Default)]
struct BudgetState {
    bytes: usize,
    allocations: usize,
    waiters: Vec<Weak<BudgetWake>>,
    reservations: Vec<Weak<TextureReservation>>,
}

pub(crate) struct TextureBudget {
    max_bytes: usize,
    max_allocations: usize,
    max_client_bytes: usize,
    max_client_allocations: usize,
    state: Mutex<BudgetState>,
}

impl TextureBudget {
    pub(crate) fn shared() -> Arc<Self> {
        static BUDGET: OnceLock<Arc<TextureBudget>> = OnceLock::new();
        BUDGET
            .get_or_init(|| {
                Self::with_client_limits(
                    MAX_TEXTURE_BYTES,
                    MAX_TEXTURE_ALLOCATIONS,
                    MAX_RENDERER_TEXTURE_BYTES,
                    MAX_RENDERER_TEXTURE_ALLOCATIONS,
                )
            })
            .clone()
    }

    #[cfg(test)]
    pub(super) fn new(max_bytes: usize, max_allocations: usize) -> Arc<Self> {
        Self::with_client_limits(max_bytes, max_allocations, max_bytes, max_allocations)
    }

    fn with_client_limits(
        max_bytes: usize,
        max_allocations: usize,
        max_client_bytes: usize,
        max_client_allocations: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            max_bytes,
            max_allocations,
            max_client_bytes,
            max_client_allocations,
            state: Mutex::new(BudgetState::default()),
        })
    }

    #[cfg(test)]
    pub(crate) fn reserve(
        self: &Arc<Self>,
        bytes: usize,
        wake: &Arc<BudgetWake>,
    ) -> Option<Arc<TextureReservation>> {
        self.try_reserve(bytes, wake).ok()
    }

    #[cfg(test)]
    pub(crate) fn try_reserve(
        self: &Arc<Self>,
        bytes: usize,
        wake: &Arc<BudgetWake>,
    ) -> Result<Arc<TextureReservation>, BudgetError> {
        self.try_reserve_replacing(bytes, wake, None)
    }

    /// `idle` must be the exclusively selected idle version being replaced.
    /// Credit it only against the local share, never against the global charge:
    /// allocation failure can then leave the old image intact and fully accounted.
    pub(crate) fn try_reserve_replacing(
        self: &Arc<Self>,
        bytes: usize,
        wake: &Arc<BudgetWake>,
        idle: Option<&Arc<TextureReservation>>,
    ) -> Result<Arc<TextureReservation>, BudgetError> {
        let mut state = self.state.lock().unwrap();
        if bytes == 0 || bytes > self.max_bytes {
            return Err(BudgetError::TooLarge);
        }
        let credit = idle.filter(|reservation| {
            Arc::ptr_eq(&reservation.budget, self)
                && Arc::ptr_eq(&reservation.client, wake)
                && !reservation.has_readers()
        });
        let normal_bytes = wake
            .bytes
            .load(Ordering::Relaxed)
            .saturating_sub(wake.oversized_bytes.load(Ordering::Relaxed))
            .saturating_sub(
                credit
                    .filter(|reservation| !reservation.oversized)
                    .map_or(0, |reservation| reservation.bytes),
            );
        let oversized_allocations = wake
            .oversized_allocations
            .load(Ordering::Relaxed)
            .saturating_sub(usize::from(
                credit.is_some_and(|reservation| reservation.oversized),
            ));
        let local_allocations = wake
            .allocations
            .load(Ordering::Relaxed)
            .saturating_sub(usize::from(credit.is_some()));
        let oversized = bytes > self.max_client_bytes;
        if bytes > self.max_bytes.saturating_sub(state.bytes)
            || state.allocations >= self.max_allocations
            || (oversized && oversized_allocations != 0)
            || (!oversized && bytes > self.max_client_bytes.saturating_sub(normal_bytes))
            || local_allocations >= self.max_client_allocations
        {
            Self::wait(&mut state, wake);
            return Err(BudgetError::Pressure);
        }
        state.bytes += bytes;
        state.allocations += 1;
        wake.bytes.fetch_add(bytes, Ordering::Relaxed);
        wake.allocations.fetch_add(1, Ordering::Relaxed);
        if oversized {
            wake.oversized_bytes.fetch_add(bytes, Ordering::Relaxed);
            wake.oversized_allocations.fetch_add(1, Ordering::Relaxed);
        }
        let reservation = Arc::new(TextureReservation {
            budget: self.clone(),
            client: wake.clone(),
            bytes,
            oversized,
            retired: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
        });
        state
            .reservations
            .retain(|reservation| reservation.strong_count() != 0);
        state.reservations.push(Arc::downgrade(&reservation));
        Ok(reservation)
    }

    pub(crate) fn has_reclaimable_readers(&self) -> bool {
        let reservations = self.state.lock().unwrap().reservations.clone();
        reservations
            .iter()
            .filter_map(Weak::upgrade)
            .any(|reservation| {
                reservation.is_retired() && reservation.readers.load(Ordering::Acquire) != 0
            })
    }

    pub(crate) fn defer(&self, wake: &Arc<BudgetWake>) {
        Self::wait(&mut self.state.lock().unwrap(), wake);
    }

    fn wait(state: &mut BudgetState, wake: &Arc<BudgetWake>) {
        if !wake.waiting.swap(true, Ordering::AcqRel) {
            state.waiters.retain(|waiter| waiter.strong_count() != 0);
            state.waiters.push(Arc::downgrade(wake));
        }
    }
}

pub(crate) struct TextureReservation {
    budget: Arc<TextureBudget>,
    client: Arc<BudgetWake>,
    pub(crate) bytes: usize,
    oversized: bool,
    retired: AtomicBool,
    readers: AtomicUsize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BudgetError {
    TooLarge,
    Pressure,
}

impl TextureReservation {
    pub(crate) fn set_retired(&self, retired: bool) {
        self.retired.store(retired, Ordering::Release);
    }

    pub(crate) fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    pub(crate) fn has_readers(&self) -> bool {
        self.readers.load(Ordering::Acquire) != 0
    }

    pub(crate) fn begin_read(&self) {
        self.readers.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn end_read(&self) {
        let previous = self.readers.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
    }
}

impl Drop for TextureReservation {
    fn drop(&mut self) {
        let waiters = {
            let mut state = self.budget.state.lock().unwrap();
            state.bytes -= self.bytes;
            state.allocations -= 1;
            self.client.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
            self.client.allocations.fetch_sub(1, Ordering::Relaxed);
            if self.oversized {
                self.client
                    .oversized_bytes
                    .fetch_sub(self.bytes, Ordering::Relaxed);
                self.client
                    .oversized_allocations
                    .fetch_sub(1, Ordering::Relaxed);
            }
            std::mem::take(&mut state.waiters)
        };
        // Never call window code under the global budget lock.
        for wake in waiters.into_iter().filter_map(|wake| wake.upgrade()) {
            if wake.waiting.swap(false, Ordering::AcqRel) {
                (wake.notify)();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn wake() -> (Arc<BudgetWake>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let callback = count.clone();
        (
            BudgetWake::new(move || {
                callback.fetch_add(1, Ordering::Relaxed);
            }),
            count,
        )
    }

    #[test]
    fn aggregate_bytes_and_allocation_count_apply_across_clients() {
        let budget = TextureBudget::new(100, 2);
        let (a, _) = wake();
        let (b, count) = wake();
        let first = budget.reserve(60, &a).unwrap();
        assert!(budget.reserve(41, &b).is_none());
        let second = budget.reserve(40, &b).unwrap();
        assert!(budget.reserve(1, &a).is_none());
        drop(first);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        let third = budget.reserve(1, &a).unwrap();
        assert!(budget.reserve(1, &b).is_none()); // Count, not bytes, is exhausted.
        drop((second, third));
        assert_eq!(budget.state.lock().unwrap().bytes, 0);
    }

    #[test]
    fn failed_allocations_roll_back_and_waiters_are_deduplicated() {
        let budget = TextureBudget::new(16, 2);
        let (wake, count) = wake();
        let reservation = budget.reserve(16, &wake).unwrap();
        for _ in 0..100 {
            assert!(budget.reserve(1, &wake).is_none());
        }
        assert_eq!(budget.state.lock().unwrap().waiters.len(), 1);
        drop(reservation);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(budget.reserve(16, &wake).is_some());
        assert!(budget.reserve(usize::MAX, &wake).is_none());
        assert!(budget.reserve(0, &wake).is_none());
        assert_eq!(budget.state.lock().unwrap().allocations, 0);
    }

    #[test]
    fn concurrent_admission_cannot_oversubscribe() {
        let budget = TextureBudget::new(8, 8);
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let budget = budget.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let (wake, _) = wake();
                    let reservation = budget.reserve(1, &wake);
                    barrier.wait();
                    reservation
                })
            })
            .collect();
        let reservations: Vec<_> = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(reservations.len(), 8);
        drop(reservations);
        assert_eq!(budget.state.lock().unwrap().bytes, 0);
    }

    #[test]
    fn reclaim_wakeup_can_reenter_admission_without_the_budget_lock() {
        let budget = TextureBudget::new(8, 1);
        let callback_budget = budget.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let callback_count = count.clone();
        let wake = BudgetWake::new(move || {
            let retry = BudgetWake::new(|| {});
            assert!(callback_budget.reserve(8, &retry).is_some());
            callback_count.fetch_add(1, Ordering::Relaxed);
        });
        let held = budget.reserve(8, &wake).unwrap();
        assert!(budget.reserve(1, &wake).is_none());
        drop(held);
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn idle_renderer_cannot_monopolize_budget_and_stalled_clients_recover() {
        let budget = TextureBudget::with_client_limits(256, 16, 64, 4);
        let (a, _) = wake();
        let (b, _) = wake();
        let idle_a = budget.try_reserve(64, &a).unwrap();
        assert!(matches!(
            budget.try_reserve(1, &a),
            Err(BudgetError::Pressure)
        ));
        assert!(!budget.has_reclaimable_readers());
        let idle_b = budget.try_reserve(64, &b).unwrap();
        let (c, _) = wake();
        let (d, _) = wake();
        let (e, count) = wake();
        let idle_c = budget.try_reserve(64, &c).unwrap();
        let idle_d = budget.try_reserve(64, &d).unwrap();
        assert!(matches!(
            budget.try_reserve(1, &e),
            Err(BudgetError::Pressure)
        ));
        assert!(!budget.has_reclaimable_readers()); // Rejected by the renderer, not deferred.
        assert!(matches!(
            budget.try_reserve(257, &e),
            Err(BudgetError::TooLarge)
        ));
        assert!(matches!(
            budget.try_reserve(65, &e),
            Err(BudgetError::Pressure)
        ));
        drop(idle_a);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert!(budget.try_reserve(64, &e).is_ok());
        drop((idle_b, idle_c, idle_d));
    }

    #[test]
    fn six_k_and_eight_k_frames_get_one_frame_shares_not_unbounded_rings() {
        let budget = TextureBudget::with_client_limits(
            MAX_TEXTURE_BYTES,
            MAX_TEXTURE_ALLOCATIONS,
            MAX_RENDERER_TEXTURE_BYTES,
            MAX_RENDERER_TEXTURE_ALLOCATIONS,
        );
        let (large, _) = wake();
        let (other, _) = wake();
        let other_frame = budget.try_reserve(32 * 1024 * 1024, &other).unwrap();
        for (width, height) in [(6016usize, 3384usize), (7680, 4320)] {
            let bytes = (width * height * 4).next_multiple_of(64 * 1024);
            assert!(bytes > MAX_RENDERER_TEXTURE_BYTES && bytes < MAX_TEXTURE_BYTES);
            let frame = budget.try_reserve(bytes, &large).unwrap();
            assert_eq!(
                budget.state.lock().unwrap().bytes,
                bytes + other_frame.bytes
            );
            assert!(matches!(
                budget.try_reserve(bytes, &large),
                Err(BudgetError::Pressure)
            ));
            let ui = budget
                .try_reserve(MAX_RENDERER_TEXTURE_BYTES, &large)
                .unwrap();
            assert!(matches!(
                budget.try_reserve(1, &large),
                Err(BudgetError::Pressure)
            ));
            drop(ui);
            drop(frame);
            assert_eq!(large.bytes.load(Ordering::Relaxed), 0);
        }
        assert!(matches!(
            budget.try_reserve(MAX_TEXTURE_BYTES + 1, &large),
            Err(BudgetError::TooLarge)
        ));
    }

    #[test]
    fn idle_resize_credit_preserves_rollback_and_never_discounts_global_bytes() {
        let budget = TextureBudget::with_client_limits(256, 16, 64, 4);
        let (owner, _) = wake();
        let old = budget.try_reserve(32, &owner).unwrap();
        let large = budget.try_reserve(80, &owner).unwrap();
        assert!(matches!(
            budget.try_reserve(81, &owner),
            Err(BudgetError::Pressure)
        ));
        drop(large);
        let replacement = budget
            .try_reserve_replacing(80, &owner, Some(&old))
            .unwrap();
        assert_eq!(budget.state.lock().unwrap().bytes, 112);
        drop(replacement); // Failed Metal allocation: retain the old image and its charge.
        assert_eq!(owner.bytes.load(Ordering::Relaxed), 32);
        let busy_large = budget.try_reserve(80, &owner).unwrap();
        busy_large.begin_read();
        assert!(matches!(
            budget.try_reserve_replacing(81, &owner, Some(&busy_large)),
            Err(BudgetError::Pressure)
        ));
        busy_large.end_read();
        drop(busy_large);
        let replacement = budget
            .try_reserve_replacing(80, &owner, Some(&old))
            .unwrap();
        drop(old); // Successful upload replaces the idle current version.
        assert_eq!(owner.bytes.load(Ordering::Relaxed), 80);
        let (other, _) = wake();
        let _other = budget.try_reserve(64, &other).unwrap();
        assert!(matches!(
            budget.try_reserve_replacing(128, &owner, Some(&replacement)),
            Err(BudgetError::Pressure)
        ));
        assert_eq!(budget.state.lock().unwrap().bytes, 144);
        let (stranger, _) = wake();
        assert!(matches!(
            budget.try_reserve_replacing(81, &owner, Some(&_other)),
            Err(BudgetError::Pressure)
        ));
        let _stranger = budget
            .try_reserve_replacing(64, &stranger, Some(&replacement))
            .unwrap();
        assert_eq!(budget.state.lock().unwrap().bytes, 208);
    }

    #[test]
    fn existing_ui_cache_does_not_block_one_oversized_frame_or_its_resize() {
        let budget = TextureBudget::with_client_limits(256, 16, 64, 4);
        let (owner, _) = wake();
        let ui = budget.try_reserve(4, &owner).unwrap();
        let frame = budget.try_reserve(80, &owner).unwrap();
        let resized = budget
            .try_reserve_replacing(128, &owner, Some(&frame))
            .unwrap();
        assert_eq!(budget.state.lock().unwrap().bytes, 212); // Both frames charged globally.
        drop(frame);
        assert_eq!(owner.oversized_bytes.load(Ordering::Relaxed), 128);
        assert_eq!(owner.oversized_allocations.load(Ordering::Relaxed), 1);
        assert!(matches!(
            budget.try_reserve(80, &owner),
            Err(BudgetError::Pressure)
        ));
        drop(resized);
        assert_eq!(owner.oversized_allocations.load(Ordering::Relaxed), 0);
        drop(ui);
        assert_eq!(owner.bytes.load(Ordering::Relaxed), 0);
    }
}
