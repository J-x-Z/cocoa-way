//! Cumulative worker-local progress, independent of the current FPS interval.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub(crate) struct RenderProgress {
    pub surface_commits: u64,
    pub submitted_content_frames: u64,
}

impl RenderProgress {
    pub fn record_submission(&mut self, submitted: bool, rendered_surfaces: usize) {
        if submitted && rendered_surfaces > 0 {
            self.submitted_content_frames = self.submitted_content_frames.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_submissions_and_empty_clears_do_not_establish_content_progress() {
        let mut progress = RenderProgress::default();
        progress.record_submission(false, 2);
        progress.record_submission(true, 0);
        assert_eq!(progress.submitted_content_frames, 0);
        progress.record_submission(true, 2);
        assert_eq!(progress.submitted_content_frames, 1);
    }

    #[test]
    fn cumulative_progress_survives_an_idle_interval_and_is_worker_local() {
        let mut first = RenderProgress::default();
        first.record_submission(true, 1);
        let second = RenderProgress::default();
        let snapshot: RenderProgress =
            serde_json::from_slice(&serde_json::to_vec(&first).unwrap()).unwrap();
        assert_eq!(snapshot.submitted_content_frames, 1);
        assert_eq!(second.submitted_content_frames, 0);
    }

    #[test]
    fn diagnostic_counter_does_not_wrap_after_saturation() {
        let mut progress = RenderProgress {
            submitted_content_frames: u64::MAX,
            ..RenderProgress::default()
        };
        progress.record_submission(true, 1);
        assert_eq!(progress.submitted_content_frames, u64::MAX);
    }
}
