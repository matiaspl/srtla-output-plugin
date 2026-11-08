//! Video shedding under congestion, so audio keeps flowing.
//!
//! Audio and video share one MPEG-TS stream and one SRT socket, and SRT
//! drops late packets without regard to content. When the bitrate controller
//! cannot keep the sender queue inside the latency budget (sudden fades,
//! capacity below the configured minimum), the output instead stops feeding
//! video into the muxer while audio continues. The picture freezes and
//! recovers at a keyframe; sound is not interrupted. Audio is a few percent
//! of the stream, so protecting it costs almost nothing.
//!
//! Decisions are made per encoded video frame, before muxing:
//! - *Disposable*: drop frames nothing else references (OBS
//!   `OBS_NAL_PRIORITY_DISPOSABLE`, e.g. non-reference B-frames). Only used
//!   once the encoder has shown it sets frame priorities.
//! - *Freeze*: drop all video. A reference frame cannot be dropped alone, so
//!   once anything is dropped the stream waits for the next keyframe.
//! - A keyframe is admitted only if sending it at the measured delivery rate
//!   keeps the queue within the budget; a burst that would arrive late is
//!   useless and only delays audio.

/// `OBS_NAL_PRIORITY_DISPOSABLE` in libobs.
pub const PRIORITY_DISPOSABLE: i32 = 0;
/// `OBS_NAL_PRIORITY_HIGHEST` in libobs.
pub const PRIORITY_HIGHEST: i32 = 3;

// Thresholds on queue pressure: queueing delay beyond the RTT floor as a
// fraction of the latency budget left after the floor.
const DISPOSABLE_ENTER: f64 = 0.35;
const DISPOSABLE_EXIT: f64 = 0.25;
const FREEZE_ENTER: f64 = 0.55;
const FREEZE_EXIT: f64 = 0.35;
/// A keyframe may raise pressure to at most this when admitted.
const KEYFRAME_LIMIT: f64 = 0.80;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShedLevel {
    #[default]
    None,
    Disposable,
    Freeze,
}

/// Transport state the decision is based on.
#[derive(Clone, Copy, Debug)]
pub struct PathState {
    /// SRT sender-queue delay (age span of unacknowledged data).
    pub queue_ms: f64,
    pub rtt_floor_ms: f64,
    pub latency_ms: f64,
    /// End-to-end delivery rate (all media), when measurable.
    pub delivery_bps: Option<u64>,
}

impl PathState {
    fn budget_ms(&self) -> f64 {
        (self.latency_ms - self.rtt_floor_ms)
            .max(self.latency_ms / 4.0)
            .max(1.0)
    }

    fn excess_ms(&self) -> f64 {
        (self.queue_ms - self.rtt_floor_ms).max(0.0)
    }

    pub fn pressure(&self) -> f64 {
        self.excess_ms() / self.budget_ms()
    }
}

#[derive(Clone, Debug, Default)]
pub struct VideoShedder {
    level: ShedLevel,
    /// A frame was dropped; dependent frames must wait for a keyframe.
    wait_keyframe: bool,
    /// The encoder marks keyframes HIGHEST, so its priorities are real (an
    /// encoder that never sets them reports 0 = disposable for everything).
    priorities_known: bool,
    pressure: f64,
    pub frames_shed: u64,
    pub keyframes_shed: u64,
    pub bytes_shed: u64,
}

impl VideoShedder {
    pub fn level(&self) -> ShedLevel {
        self.level
    }

    pub fn pressure(&self) -> f64 {
        self.pressure
    }

    /// True while video is being withheld (paused until a keyframe or
    /// shedding everything).
    pub fn video_paused(&self) -> bool {
        self.wait_keyframe || self.level == ShedLevel::Freeze
    }

    pub fn state_label(&self) -> &'static str {
        if self.video_paused() {
            "Video paused"
        } else if self.level == ShedLevel::Disposable {
            "Dropping disposable frames"
        } else {
            ""
        }
    }

    fn update_level(&mut self, path: Option<&PathState>) {
        let Some(path) = path else {
            // No current feedback: do not shed on stale evidence.
            self.level = ShedLevel::None;
            self.pressure = 0.0;
            return;
        };
        let x = path.pressure();
        self.pressure = x;
        self.level = match self.level {
            ShedLevel::None if x >= FREEZE_ENTER => ShedLevel::Freeze,
            ShedLevel::None if x >= DISPOSABLE_ENTER => ShedLevel::Disposable,
            ShedLevel::None => ShedLevel::None,
            ShedLevel::Disposable if x >= FREEZE_ENTER => ShedLevel::Freeze,
            ShedLevel::Disposable if x < DISPOSABLE_EXIT => ShedLevel::None,
            ShedLevel::Disposable => ShedLevel::Disposable,
            ShedLevel::Freeze if x < DISPOSABLE_EXIT => ShedLevel::None,
            ShedLevel::Freeze if x < FREEZE_EXIT => ShedLevel::Disposable,
            ShedLevel::Freeze => ShedLevel::Freeze,
        };
    }

    fn keyframe_fits(&self, path: Option<&PathState>, size: usize) -> bool {
        if self.level == ShedLevel::None {
            return true;
        }
        let Some(path) = path else {
            return true;
        };
        let Some(delivery) = path.delivery_bps.filter(|d| *d > 0) else {
            return false;
        };
        let send_ms = size as f64 * 8_000.0 / delivery as f64;
        (path.excess_ms() + send_ms) / path.budget_ms() <= KEYFRAME_LIMIT
    }

    /// Decide whether one encoded video frame is sent. Audio never passes
    /// through here.
    pub fn admit(
        &mut self,
        path: Option<&PathState>,
        size: usize,
        keyframe: bool,
        drop_priority: i32,
    ) -> bool {
        self.update_level(path);
        let admit = if keyframe {
            if drop_priority >= PRIORITY_HIGHEST {
                self.priorities_known = true;
            }
            let fits = self.keyframe_fits(path, size);
            self.wait_keyframe = !fits;
            if !fits {
                self.keyframes_shed += 1;
            }
            fits
        } else if self.wait_keyframe {
            false
        } else {
            match self.level {
                ShedLevel::None => true,
                ShedLevel::Disposable => {
                    !(self.priorities_known && drop_priority <= PRIORITY_DISPOSABLE)
                }
                ShedLevel::Freeze => {
                    self.wait_keyframe = true;
                    false
                }
            }
        };
        if !admit {
            self.frames_shed += 1;
            self.bytes_shed += size as u64;
        }
        admit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: i32 = 1;

    fn path(queue_ms: f64, delivery_bps: Option<u64>) -> PathState {
        PathState {
            queue_ms,
            rtt_floor_ms: 60.0,
            latency_ms: 2_060.0,
            delivery_bps,
        }
    }

    fn known_priorities(shedder: &mut VideoShedder) {
        assert!(shedder.admit(
            Some(&path(60.0, Some(4_000_000))),
            50_000,
            true,
            PRIORITY_HIGHEST
        ));
    }

    #[test]
    fn healthy_path_sends_everything() {
        let mut s = VideoShedder::default();
        known_priorities(&mut s);
        let p = path(100.0, Some(4_000_000));
        for _ in 0..30 {
            assert!(s.admit(Some(&p), 10_000, false, PRIORITY_DISPOSABLE));
            assert!(s.admit(Some(&p), 10_000, false, LOW));
        }
        assert_eq!(s.frames_shed, 0);
        assert_eq!(s.level(), ShedLevel::None);
    }

    #[test]
    fn moderate_pressure_drops_only_disposable_frames() {
        let mut s = VideoShedder::default();
        known_priorities(&mut s);
        // 800 ms excess of a 2 s budget = 0.4.
        let p = path(860.0, Some(4_000_000));
        assert!(!s.admit(Some(&p), 8_000, false, PRIORITY_DISPOSABLE));
        assert!(s.admit(Some(&p), 20_000, false, LOW));
        assert_eq!(s.level(), ShedLevel::Disposable);
        assert!(!s.video_paused());
    }

    #[test]
    fn unknown_priorities_are_never_treated_as_disposable() {
        let mut s = VideoShedder::default();
        // Encoder reports 0 for everything, keyframes included.
        assert!(s.admit(Some(&path(60.0, Some(4_000_000))), 50_000, true, 0));
        let p = path(860.0, Some(4_000_000));
        assert!(s.admit(Some(&p), 8_000, false, 0));
    }

    #[test]
    fn high_pressure_pauses_video_until_a_keyframe_that_fits() {
        let mut s = VideoShedder::default();
        known_priorities(&mut s);
        let congested = path(1_400.0, Some(1_000_000));
        assert!(!s.admit(Some(&congested), 10_000, false, LOW));
        assert!(s.video_paused());
        // A 150 KB keyframe at 1 Mb/s needs 1.2 s: would arrive late.
        assert!(!s.admit(Some(&congested), 150_000, true, PRIORITY_HIGHEST));
        assert_eq!(s.keyframes_shed, 1);
        // Queue drained below the exit threshold, but frames still depend
        // on the dropped ones until the next keyframe.
        let drained = path(200.0, Some(1_000_000));
        assert!(!s.admit(Some(&drained), 10_000, false, LOW));
        assert!(s.admit(Some(&drained), 150_000, true, PRIORITY_HIGHEST));
        assert!(s.admit(Some(&drained), 10_000, false, LOW));
        assert!(!s.video_paused());
    }

    #[test]
    fn small_keyframes_pass_during_a_freeze_as_a_slideshow() {
        let mut s = VideoShedder::default();
        known_priorities(&mut s);
        let congested = path(1_300.0, Some(3_000_000));
        assert!(!s.admit(Some(&congested), 10_000, false, LOW));
        // 30 KB at 3 Mb/s = 80 ms: fits within the budget.
        assert!(s.admit(Some(&congested), 30_000, true, PRIORITY_HIGHEST));
        // Still congested: the next delta frame is dropped again.
        assert!(!s.admit(Some(&congested), 10_000, false, LOW));
    }

    #[test]
    fn levels_have_hysteresis() {
        let mut s = VideoShedder::default();
        known_priorities(&mut s);
        s.admit(Some(&path(1_300.0, Some(3_000_000))), 1, false, LOW);
        assert_eq!(s.level(), ShedLevel::Freeze);
        // 0.45: below freeze entry but above its exit.
        s.admit(Some(&path(960.0, Some(3_000_000))), 1, false, LOW);
        assert_eq!(s.level(), ShedLevel::Freeze);
        s.admit(Some(&path(660.0, Some(3_000_000))), 1, false, LOW);
        assert_eq!(s.level(), ShedLevel::Disposable);
        s.admit(Some(&path(400.0, Some(3_000_000))), 1, false, LOW);
        assert_eq!(s.level(), ShedLevel::None);
    }

    #[test]
    fn missing_feedback_never_sheds_new_frames() {
        let mut s = VideoShedder::default();
        known_priorities(&mut s);
        assert!(s.admit(None, 10_000, false, PRIORITY_DISPOSABLE));
        assert!(s.admit(None, 150_000, true, PRIORITY_HIGHEST));
    }

    #[test]
    fn rtt_floor_is_not_counted_against_the_budget() {
        // 600 ms RTT satellite path with 2.4 s latency, no queueing.
        let p = PathState {
            queue_ms: 610.0,
            rtt_floor_ms: 600.0,
            latency_ms: 2_400.0,
            delivery_bps: Some(4_000_000),
        };
        assert!(p.pressure() < 0.01);
    }
}
