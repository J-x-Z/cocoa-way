//! Demand-driven frame pacing, with one clock per native window.
//!
//! The caller owns dirty state, pending callbacks and redraw requests. This helper
//! only supplies monotonic deadlines; it never creates a timer or polls a window.

use std::time::{Duration, Instant};

const DEFAULT_REFRESH_MILLIHERTZ: u32 = 60_000;

#[derive(Debug)]
pub struct FrameClock {
    refresh_millihertz: u32,
    phase_anchor: Option<Instant>,
    last_frame_time: Option<Instant>,
}

impl FrameClock {
    pub fn new(refresh_millihertz: Option<u32>) -> Self {
        let mut clock = Self {
            refresh_millihertz: DEFAULT_REFRESH_MILLIHERTZ,
            phase_anchor: None,
            last_frame_time: None,
        };
        clock.set_refresh_millihertz(refresh_millihertz);
        clock
    }

    /// Missing or implausible monitor rates outside 1-1000 Hz use 60 Hz.
    /// Updates preserve the phase anchor and frame progress. The next deadline
    /// is a slot at the new rate strictly after the latest frame milestone.
    pub fn set_refresh_millihertz(&mut self, refresh_millihertz: Option<u32>) {
        self.refresh_millihertz = refresh_millihertz
            .filter(|rate| (1_000..=1_000_000).contains(rate))
            .unwrap_or(DEFAULT_REFRESH_MILLIHERTZ);
    }

    pub fn refresh_millihertz(&self) -> u32 {
        self.refresh_millihertz
    }

    /// Pass dirty-frame OR pending-callback demand for this window as `has_work`.
    /// Idle returns no deadline. New or overdue work is due now. Deadlines stay
    /// phase-anchored, so render cost and wakeup jitter do not accumulate.
    /// Querying never consumes work or replays missed slots.
    /// Suppress scheduling while a requested redraw is already pending.
    pub fn next_due(&self, now: Instant, has_work: bool) -> Option<Instant> {
        if !has_work {
            return None;
        }

        let due = match (self.phase_anchor, self.last_frame_time) {
            (Some(anchor), Some(last)) => {
                // Find the next phase slot in constant time, even after a long
                // idle/render. Round up the period to avoid exceeding the rate.
                let period_ns = 1_000_000_000_000_u64.div_ceil(u64::from(self.refresh_millihertz));
                let offset_ns =
                    (last.duration_since(anchor).as_nanos() % u128::from(period_ns)) as u64;
                last + Duration::from_nanos(period_ns - offset_ns)
            }
            _ => now,
        };
        Some(due.max(now))
    }

    /// Call immediately before clear/render, including callback/cursor ticks.
    /// The first start anchors the phase; later starts consume elapsed slots
    /// without resetting that phase. Older timestamps cannot rewind progress.
    pub fn record_frame_started(&mut self, started_at: Instant) {
        self.phase_anchor.get_or_insert(started_at);
        self.last_frame_time = Some(
            self.last_frame_time
                .map_or(started_at, |last| last.max(started_at)),
        );
    }

    /// Call after actual submission, paired with `record_frame_started` before
    /// rendering. This skips slots elapsed during rendering, but does not add a
    /// whole period to the submission time. Without a prior start, this seeds
    /// the phase at submission time instead.
    pub fn record_submission(&mut self, submitted_at: Instant) {
        self.record_frame_started(submitted_at);
    }
}

impl Default for FrameClock {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_three_ms_renders_keep_sixty_and_one_twenty_hz_phase() {
        let anchor = Instant::now();
        let render_time = Duration::from_millis(3);

        for (refresh, period_ns) in [(60_000, 16_666_667), (120_000, 8_333_334)] {
            let mut clock = FrameClock::new(Some(refresh));
            let period = Duration::from_nanos(period_ns);
            for slot in 0..120_u32 {
                let started_at = anchor + period * slot;
                let next_slot = anchor + period * (slot + 1);
                assert_eq!(clock.next_due(started_at, true), Some(started_at));
                clock.record_frame_started(started_at);
                assert_eq!(clock.next_due(started_at, true), Some(next_slot));
                let submitted_at = started_at + render_time;
                clock.record_submission(submitted_at);
                assert_eq!(clock.next_due(submitted_at, true), Some(next_slot));
            }
        }
    }

    #[test]
    fn sixty_hz_deadline_does_not_accumulate_render_or_wakeup_cost() {
        let now = Instant::now();
        let mut clock = FrameClock::new(Some(60_000));
        let period = Duration::from_nanos(16_666_667);

        assert_eq!(clock.next_due(now, true), Some(now));
        clock.record_frame_started(now);
        let submitted_at = now + Duration::from_millis(3);
        clock.record_submission(submitted_at);
        assert_eq!(clock.next_due(submitted_at, true), Some(now + period));
        assert_eq!(
            clock.next_due(now + Duration::from_millis(16), true),
            Some(now + period),
        );
        let late_start = now + period + Duration::from_millis(1);
        assert_eq!(clock.next_due(late_start, true), Some(late_start));
        clock.record_frame_started(late_start);
        let late_submission = late_start + Duration::from_millis(3);
        clock.record_submission(late_submission);
        assert_eq!(
            clock.next_due(late_submission, true),
            Some(now + period * 2),
        );
    }

    #[test]
    fn one_twenty_hz_and_sixty_hz_windows_are_independent() {
        let now = Instant::now();
        let mut fast = FrameClock::new(Some(120_000));
        let mut slow = FrameClock::new(Some(60_000));
        let fast_interval = Duration::from_nanos(8_333_334);
        let slow_interval = Duration::from_nanos(16_666_667);

        fast.record_frame_started(now);
        slow.record_frame_started(now);
        fast.record_submission(now + Duration::from_millis(3));
        slow.record_submission(now + Duration::from_millis(3));
        assert_eq!(fast.next_due(now, true), Some(now + fast_interval));
        assert_eq!(slow.next_due(now, true), Some(now + slow_interval));
        fast.record_frame_started(now + fast_interval);
        fast.record_submission(now + fast_interval + Duration::from_millis(3));
        assert_eq!(
            fast.next_due(now + fast_interval, true),
            Some(now + fast_interval * 2),
        );
        assert_eq!(slow.next_due(now, true), Some(now + slow_interval));
    }

    #[test]
    fn idle_has_no_deadline_even_after_submission_or_refresh_change() {
        let now = Instant::now();
        let mut clock = FrameClock::default();

        assert_eq!(clock.next_due(now, false), None);
        clock.record_frame_started(now);
        clock.record_submission(now + Duration::from_millis(3));
        assert_eq!(clock.next_due(now, false), None);
        clock.set_refresh_millihertz(Some(120_000));
        assert_eq!(clock.next_due(now + Duration::from_secs(10), false), None);
        let resumed_at = now + Duration::from_secs(10);
        assert_eq!(clock.next_due(resumed_at, true), Some(resumed_at));
    }

    #[test]
    fn long_idle_demand_resumes_once_and_skips_expired_phase_slots() {
        let anchor = Instant::now();
        let period = Duration::from_nanos(16_666_667);
        let mut clock = FrameClock::default();
        clock.record_frame_started(anchor);
        clock.record_submission(anchor + Duration::from_millis(3));
        let resumed_at = anchor + period * 1_000_000 + Duration::from_millis(5);

        assert_eq!(clock.next_due(resumed_at, false), None);
        assert_eq!(clock.next_due(resumed_at, true), Some(resumed_at));
        clock.record_frame_started(resumed_at);
        clock.record_submission(resumed_at + Duration::from_millis(3));
        assert_eq!(
            clock.next_due(resumed_at + Duration::from_millis(3), true),
            Some(anchor + period * 1_000_001),
        );
    }

    #[test]
    fn callbacks_without_a_dirty_frame_still_have_a_paced_deadline() {
        let now = Instant::now();
        let mut clock = FrameClock::default();
        let dirty = false;
        let callbacks_pending = true;

        assert_eq!(clock.next_due(now, dirty || callbacks_pending), Some(now));
        clock.record_frame_started(now);
        clock.record_submission(now + Duration::from_millis(3));
        assert_eq!(
            clock.next_due(now, dirty || callbacks_pending),
            Some(now + Duration::from_nanos(16_666_667)),
        );
        assert_eq!(clock.next_due(now, dirty), None);
    }

    #[test]
    fn refresh_changes_preserve_phase_and_never_replay_consumed_slots() {
        let now = Instant::now();
        let mut clock = FrameClock::new(Some(60_000));
        let slow_period = Duration::from_nanos(16_666_667);
        let fast_period = Duration::from_nanos(8_333_334);
        clock.record_frame_started(now);
        clock.record_submission(now + Duration::from_millis(3));
        clock.record_frame_started(now + slow_period);
        let sampled_at = now + slow_period + Duration::from_millis(3);
        clock.record_submission(sampled_at);

        clock.set_refresh_millihertz(Some(120_000));
        assert_eq!(clock.refresh_millihertz(), 120_000);
        let fast_due = now + fast_period * 3;
        assert_eq!(clock.next_due(sampled_at, true), Some(fast_due));
        clock.set_refresh_millihertz(Some(120_000));
        assert_eq!(clock.next_due(sampled_at, true), Some(fast_due));

        clock.set_refresh_millihertz(Some(60_000));
        assert_eq!(
            clock.next_due(sampled_at, true),
            Some(now + slow_period * 2),
        );
        clock.set_refresh_millihertz(Some(120_000));
        let late = now + Duration::from_millis(26);
        assert_eq!(clock.next_due(late, true), Some(late));
        clock.record_frame_started(late);
        let submitted_at = late + Duration::from_millis(3);
        clock.record_submission(submitted_at);
        for rate in [Some(60_000), Some(120_000), None, Some(0)] {
            clock.set_refresh_millihertz(rate);
            assert!(clock.next_due(submitted_at, true).unwrap() > submitted_at);
            assert_eq!(clock.next_due(submitted_at, false), None);
        }
    }

    #[test]
    fn missed_frames_are_skipped_without_resetting_phase_or_catchup() {
        let now = Instant::now();
        let mut clock = FrameClock::default();
        clock.record_frame_started(now);
        clock.record_submission(now + Duration::from_millis(3));
        let late = now + Duration::from_millis(105);

        assert_eq!(clock.next_due(late, true), Some(late));
        assert_eq!(clock.next_due(late, true), Some(late));
        clock.record_frame_started(late);
        let next = now + Duration::from_nanos(16_666_667) * 7;
        assert_eq!(clock.next_due(late, true), Some(next));
        clock.record_submission(late + Duration::from_millis(3));
        assert_eq!(
            clock.next_due(late + Duration::from_millis(3), true),
            Some(next),
        );
    }

    #[test]
    fn long_render_skips_all_elapsed_slots_without_immediate_redraw() {
        let now = Instant::now();
        let mut clock = FrameClock::default();
        clock.record_frame_started(now);
        let submitted_at = now + Duration::from_millis(90);
        clock.record_submission(submitted_at);

        assert_eq!(
            clock.next_due(submitted_at, true),
            Some(now + Duration::from_nanos(16_666_667) * 6),
        );
    }

    #[test]
    fn stale_frame_timestamps_do_not_move_progress_backwards() {
        let now = Instant::now();
        let mut clock = FrameClock::default();
        let submitted_at = now + Duration::from_millis(20);

        clock.record_frame_started(now);
        clock.record_submission(submitted_at);
        clock.record_frame_started(now);
        clock.record_submission(now);
        assert_eq!(
            clock.next_due(submitted_at, true),
            Some(now + Duration::from_nanos(16_666_667) * 2),
        );
    }

    #[test]
    fn unknown_and_invalid_refresh_rates_fall_back_to_sixty_hz() {
        let now = Instant::now();
        for rate in [None, Some(0), Some(999), Some(1_000_001), Some(u32::MAX)] {
            let mut clock = FrameClock::new(rate);
            assert_eq!(clock.refresh_millihertz(), 60_000);
            clock.record_submission(now);
            assert_eq!(
                clock.next_due(now, true),
                Some(now + Duration::from_nanos(16_666_667)),
            );
        }
    }

    #[test]
    fn valid_refresh_boundaries_and_fractional_rates_are_preserved() {
        let now = Instant::now();
        for (rate, nanoseconds) in [
            (1_000, 1_000_000_000),
            (1_000_000, 1_000_000),
            (59_940, 16_683_351),
        ] {
            let mut clock = FrameClock::new(Some(rate));
            assert_eq!(clock.refresh_millihertz(), rate);
            clock.record_submission(now);
            assert_eq!(
                clock.next_due(now, true),
                Some(now + Duration::from_nanos(nanoseconds)),
            );
        }
    }
}
