//! Cooperative background-work admission. Native input never drains control work
//! first; pending messages retain their existing FIFO and AboutToWait wakeup.

use std::time::{Duration, Instant};

pub struct ControlWorkBudget {
    input_turn: bool,
    deadline: Instant,
    admitted: usize,
}

impl ControlWorkBudget {
    pub fn new(input_turn: bool, now: Instant) -> Self {
        Self {
            input_turn,
            deadline: now + Duration::from_millis(2),
            admitted: 0,
        }
    }

    // This bounds work between handlers, not the duration of a single handler.
    pub fn admit(&mut self, now: Instant) -> bool {
        if self.input_turn || self.admitted == 64 || now >= self.deadline {
            return false;
        }
        self.admitted += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_bypasses_background_work_without_consuming_or_losing_its_wakeup() {
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        let now = Instant::now();
        let mut input = ControlWorkBudget::new(true, now);
        assert!(!input.admit(now));
        assert!(
            sender.finish_turn(),
            "unconsumed work must retain a continuation deadline"
        );
        let mut background = ControlWorkBudget::new(false, now);
        let mut received = Vec::new();
        while background.admit(now) {
            let Ok(message) = receiver.try_recv() else {
                break;
            };
            sender.message_received();
            received.push(message);
        }
        assert_eq!(received, [1, 2]);
        assert!(!sender.finish_turn());
    }

    #[test]
    fn count_and_time_budgets_are_checked_between_handlers() {
        let now = Instant::now();
        let mut budget = ControlWorkBudget::new(false, now);
        for _ in 0..64 {
            assert!(budget.admit(now));
        }
        assert!(!budget.admit(now));
        let mut budget = ControlWorkBudget::new(false, now);
        assert!(budget.admit(now));
        assert!(!budget.admit(now + Duration::from_millis(2)));
    }
}
