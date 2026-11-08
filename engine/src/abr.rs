use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

// Closed-loop encoder bitrate controller.
//
// Signals: end-to-end SRT RTT, SRT sender-queue delay, and the end-to-end
// delivery rate (unique payload acknowledged by the receiver), all sampled
// from the SRT socket every 20 ms. Per-link SRTLA CC targets and the SRT
// packet-pair bandwidth estimate remain diagnostics only.
//
// Control law, in short:
// - congestion is classified as light / heavy / severe against learned
//   windowed-minimum baselines (not moving averages that normalise a
//   standing queue);
// - decreases are sized from the measured delivery rate, so one cut lands
//   near the real capacity instead of a staircase of fixed steps;
// - after a cut the controller waits for the cut to propagate through the
//   encoder and the network (settle) before cutting again, and holds
//   increases until the queue has drained;
// - the bitrate at which congestion occurred is remembered (ceiling). Far
//   below it the controller ramps multiplicatively; near it it approaches in
//   small steps, dwells, then probes above it with gradually growing steps.
//   This removes the periodic sawtooth caused by repeatedly ramping into the
//   same capacity limit.

const DEFAULT_MIN_BPS: u64 = 500_000;
const DEFAULT_START_BPS: u64 = 1_500_000;
const ROUNDING_BPS: u64 = 100_000;
pub(crate) const SRT_PAYLOAD_BYTES: f64 = 1_316.0;

// Estimator windows.
const HISTORY_MS: u64 = 2_500;
const BASELINE_WINDOW_MS: u64 = 10_000;
const BASELINE_BUCKET_MS: u64 = 500;
const DELIVERY_WINDOW_MS: u64 = 500;
const TREND_SPAN_MS: u64 = 200;

// Increase law.
const FAST_INCREASE_INTERVAL_MS: u64 = 250;
const FAST_INCREASE_PERMILLE: u64 = 100;
/// With a known ceiling, close at least this fraction of the remaining gap to
/// the end of the fast zone per step (so a deep cut recovers in ~1 s).
const FAST_GAP_DIVISOR: u64 = 4;
const FAST_INCREASE_MIN_BPS: u64 = 50_000;
const FAST_ZONE_PERMILLE: u64 = 850;
const FAST_ZONE_CAP_PERMILLE: u64 = 900;
const APPROACH_INCREASE_INTERVAL_MS: u64 = 500;
const APPROACH_INCREASE_PERMILLE: u64 = 15;
const APPROACH_ZONE_PERMILLE: u64 = 970;
const SMALL_INCREASE_MIN_BPS: u64 = 30_000;
const PROBE_DWELL_MS: u64 = 3_000;
/// When congestion keeps confirming the same ceiling, probe above it less
/// often (dwell doubles up to this limit).
const PROBE_DWELL_MAX_MS: u64 = 24_000;
const SAME_CEILING_PERMILLE: u64 = 70;
/// Must exceed encoder lag + RTT so each probe's effect is observed before
/// the next one.
const PROBE_INTERVAL_MS: u64 = 1_000;
const PROBE_INITIAL_PERMILLE: u64 = 20;
/// Capacity memory: the highest delivery rate seen recently. On a moving
/// link a ceiling learned in a fade is stale within seconds; when the
/// controller sits at a ceiling well below this memory it retries higher.
const MEMORY_WINDOW_MS: u64 = 30_000;
const MEMORY_BUCKET_MS: u64 = 1_000;
const MEMORY_RETRY_ABOVE_PERMILLE: u64 = 1_300;
/// Each retry at most doubles the ceiling (bounding a failed retry's
/// overshoot), capped at the memory.
const MEMORY_RETRY_FACTOR: u64 = 2;
const RESTORE_DWELL_MS: u64 = 1_500;
const RESTORE_DWELL_MAX_MS: u64 = 12_000;
/// Congestion this soon after a retry, well below the retried ceiling,
/// means the retry failed.
const RESTORE_GRACE_MS: u64 = 5_000;
const RESTORE_FAILED_BELOW_PERMILLE: u64 = 700;
const PROBE_MAX_PERMILLE: u64 = 80;

// Decrease law.
const DECREASE_MIN_BPS: u64 = 100_000;
const LIGHT_DECREASE_PERMILLE: u64 = 50;
const HEAVY_DECREASE_PERMILLE: u64 = 100;
const LIGHT_DELIVERY_FACTOR: f64 = 0.95;
const HEAVY_DELIVERY_FACTOR: f64 = 0.90;
const SEVERE_DELIVERY_FACTOR: f64 = 0.80;
const HEAVY_DELIVERY_FLOOR: f64 = 0.50;
const SEVERE_DELIVERY_FLOOR: f64 = 0.35;
const DRAIN_TIME_MS: u64 = 1_500;
const ESCALATE_BELOW_PERMILLE: u64 = 800;
const ENCODER_LAG_MS: u64 = 400;
const SETTLE_MIN_MS: u64 = 300;
const SETTLE_MAX_MS: u64 = 1_500;
const INCREASE_HOLD_MIN_MS: u64 = 1_000;
const STARTUP_HOLD_MS: u64 = 1_000;
const ELEVATED_PERSIST_MS: u64 = 1_500;
/// Delivery below this fraction of what was offered is a stall (handover,
/// momentary outage), not a capacity measurement.
const STALL_RATIO: f64 = 0.35;
/// Short window so a sub-second stall is recognised while it happens.
const STALL_WINDOW_MS: u64 = 120;
/// A stall episode that has drained within this time is restored to the
/// pre-stall bitrate and ceiling instead of being learned.
const STALL_RESTORE_WINDOW_MS: u64 = 2_000;
const STALL_RESTORE_PERMILLE: u64 = 900;
/// How long a baseline check waits for its cut to drain a standing queue.
const BASELINE_CHECK_MS: u64 = 1_000;
/// If RTT excess over the floor shrank by less than this share during the
/// check, the elevation was a base-RTT shift, not a queue.
const BASELINE_SHIFT_KEEP: f64 = 0.75;
/// Cuts made this recently before a detected base-RTT shift are undone.
const BASELINE_UNDO_WINDOW_MS: u64 = 6_000;
/// Feedback needed after a reset before the first increase.
const WARMUP_MS: u64 = 200;

const STATE_DISCONNECTED: &str = "Disconnected";
const STATE_WAITING: &str = "Waiting for SRT feedback";
const STATE_HOLD: &str = "Hold";
const STATE_SETTLING: &str = "Settling";
const STATE_BASELINE_CHECK: &str = "Baseline check";
const STATE_STALL: &str = "Link stall";
const STATE_RESTORED: &str = "Restored after stall";
const STATE_INCREASING: &str = "Increasing";
const STATE_PROBING: &str = "Probing";
const STATE_LIGHT_CONGESTION: &str = "Light congestion";
const STATE_HEAVY_CONGESTION: &str = "Heavy congestion";
const STATE_SEVERE_CONGESTION: &str = "Severe congestion";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AbrConfig {
    #[serde(default = "default_min_bps")]
    pub min_bps: u64,
    #[serde(default = "default_start_bps")]
    pub start_bps: u64,
    pub max_bps: u64,
}

impl Default for AbrConfig {
    fn default() -> Self {
        Self {
            min_bps: DEFAULT_MIN_BPS,
            start_bps: DEFAULT_START_BPS,
            max_bps: DEFAULT_START_BPS,
        }
    }
}

fn default_min_bps() -> u64 {
    DEFAULT_MIN_BPS
}

fn default_start_bps() -> u64 {
    DEFAULT_START_BPS
}

impl AbrConfig {
    pub fn normalized(&self) -> Self {
        let max_bps = self.max_bps.max(ROUNDING_BPS);
        let min_bps = self.min_bps.min(max_bps);
        Self {
            min_bps,
            start_bps: self.start_bps.clamp(min_bps, max_bps),
            max_bps,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LinkCapacity {
    pub enabled: bool,
    pub payload_eligible: bool,
    pub capacity_ready: bool,
    pub target_bps: u64,
    #[serde(default)]
    pub delivered_bps: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SrtCapacity {
    pub ready: bool,
    pub sampled_at_ms: u64,
    pub bandwidth_bps: u64,
    pub send_rate_bps: u64,
    pub send_buffer_ms: u32,
    pub send_buffer_packets: u32,
    pub rtt_ms: u32,
    pub latency_ms: u32,
    pub retransmit_permille: u32,
    pub dropped_bytes: u64,
    /// Unique payload bytes sent during the sampling interval.
    #[serde(default)]
    pub sent_unique_bytes: u64,
    /// Packets sent but not yet acknowledged at the sampling instant.
    #[serde(default)]
    pub packets_in_flight: u32,
    /// Packets reported lost (NAKed) during the sampling interval.
    #[serde(default)]
    pub sender_loss_packets: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AbrSample {
    pub links: Vec<LinkCapacity>,
    pub audio_bps: u64,
    pub current_video_bps: u64,
    pub now_ms: u64,
    pub capacity_ready: bool,
    pub all_links_down: bool,
    #[serde(default)]
    pub srt: SrtCapacity,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AbrDecision {
    /// Sum of SRTLA per-link CC targets. Diagnostic only: link CC schedules
    /// traffic, it does not prove end-to-end capacity.
    pub link_capacity_bps: u64,
    /// Measured end-to-end delivery rate (all media, acknowledged unique
    /// payload) over the last half second. Zero until measurable.
    pub srt_capacity_bps: u64,
    /// Remembered video bitrate at which congestion last occurred. Zero when
    /// unknown.
    pub estimated_capacity_bps: u64,
    pub media_budget_bps: u64,
    pub recommended_bps: u64,
    pub applied_bps: u64,
    pub changed: bool,
    pub emergency: bool,
    pub srt_limited: bool,
    pub transport_stressed: bool,
    pub control_state: String,
    pub rtt_floor_ms: u32,
    pub rtt_increase_below_ms: u32,
    pub rtt_decrease_above_ms: u32,
    pub rtt_heavy_above_ms: u32,
    pub queue_light_ms: u32,
    pub queue_heavy_ms: u32,
    pub queue_severe_ms: u32,
}

#[derive(Clone, Copy, Debug)]
struct HistoryPoint {
    t_ms: u64,
    rtt_ms: f64,
    sent_bytes: u64,
    flight: u32,
    loss: u32,
    queue_ms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Delivery {
    /// Acknowledged unique payload rate over the window.
    pub goodput_bps: u64,
    /// Unique payload offered over the same-length window, shifted back by
    /// one RTT so that it is comparable to what has been acknowledged. Zero
    /// when the history does not reach back far enough.
    pub offered_bps: u64,
}

impl Delivery {
    pub fn ratio(&self) -> f64 {
        if self.offered_bps == 0 {
            1.0
        } else {
            self.goodput_bps as f64 / self.offered_bps as f64
        }
    }
}

/// Shared signal estimation for the controller and the startup probe.
#[derive(Clone, Debug, Default)]
pub(crate) struct Estimator {
    history: VecDeque<HistoryPoint>,
    rtt_buckets: VecDeque<(u64, f64)>,
    queue_buckets: VecDeque<(u64, f64)>,
    rtt_fast: f64,
    rtt_dev: f64,
    seen_measured_rtt: bool,
    /// (bucket start, max delivery rate) over MEMORY_WINDOW_MS.
    delivery_max: VecDeque<(u64, u64)>,
}

fn push_bucket_min(buckets: &mut VecDeque<(u64, f64)>, now_ms: u64, value: f64) {
    let start = now_ms / BASELINE_BUCKET_MS * BASELINE_BUCKET_MS;
    match buckets.back_mut() {
        Some((bucket, minimum)) if *bucket == start => *minimum = minimum.min(value),
        _ => buckets.push_back((start, value)),
    }
    while buckets
        .front()
        .is_some_and(|(bucket, _)| bucket + BASELINE_WINDOW_MS <= now_ms)
    {
        buckets.pop_front();
    }
}

impl Estimator {
    pub fn push(&mut self, now_ms: u64, srt: &SrtCapacity) {
        let rtt = f64::from(srt.rtt_ms);
        // SRT reports its 100 ms initial value until the first ACKACK; do not
        // let it define the RTT floor.
        if srt.rtt_ms != 100 {
            self.seen_measured_rtt = true;
        }
        if self.rtt_fast == 0.0 {
            self.rtt_fast = rtt;
        } else {
            self.rtt_fast += (rtt - self.rtt_fast) * 0.1;
        }
        // Track path noise, not congestion: an RTT excursion caused by our
        // own queue must not widen the thresholds that detect it.
        let floor = self.rtt_floor_ms();
        let congested = self.seen_measured_rtt
            && !self.rtt_buckets.is_empty()
            && rtt > floor + 40.0_f64.max(floor * 0.4).max(self.rtt_dev * 5.0);
        if !congested {
            self.rtt_dev += ((rtt - self.rtt_fast).abs() - self.rtt_dev) * 0.05;
        }
        if self.seen_measured_rtt {
            push_bucket_min(&mut self.rtt_buckets, now_ms, rtt);
            push_bucket_min(
                &mut self.queue_buckets,
                now_ms,
                f64::from(srt.send_buffer_ms),
            );
        }
        self.history.push_back(HistoryPoint {
            t_ms: now_ms,
            rtt_ms: rtt,
            sent_bytes: srt.sent_unique_bytes,
            flight: srt.packets_in_flight,
            loss: srt.sender_loss_packets,
            queue_ms: f64::from(srt.send_buffer_ms),
        });
        while self
            .history
            .front()
            .is_some_and(|p| p.t_ms + HISTORY_MS < now_ms)
        {
            self.history.pop_front();
        }
        if let Some(d) = self.delivery(DELIVERY_WINDOW_MS, 0) {
            let start = now_ms / MEMORY_BUCKET_MS * MEMORY_BUCKET_MS;
            match self.delivery_max.back_mut() {
                Some((bucket, max)) if *bucket == start => *max = (*max).max(d.goodput_bps),
                _ => self.delivery_max.push_back((start, d.goodput_bps)),
            }
            while self
                .delivery_max
                .front()
                .is_some_and(|(bucket, _)| bucket + MEMORY_WINDOW_MS <= now_ms)
            {
                self.delivery_max.pop_front();
            }
        }
    }

    /// Highest delivery rate (all media) seen in the last 30 s.
    pub fn delivery_memory_bps(&self) -> Option<u64> {
        self.delivery_max.iter().map(|(_, v)| *v).max()
    }

    /// Adopt the current RTT and queue as the new floors (the path's base
    /// RTT changed, e.g. after a handover).
    pub fn rebase(&mut self, now_ms: u64, queue_ms: f64) {
        self.rtt_buckets.clear();
        self.queue_buckets.clear();
        push_bucket_min(&mut self.rtt_buckets, now_ms, self.rtt_fast);
        push_bucket_min(&mut self.queue_buckets, now_ms, queue_ms);
    }

    /// Forget short-term history but keep the learned RTT/queue baselines.
    pub fn carry_baselines(&self) -> Self {
        Self {
            history: VecDeque::new(),
            ..self.clone()
        }
    }

    /// Time covered by the retained history.
    pub fn history_span_ms(&self) -> u64 {
        match (self.history.front(), self.history.back()) {
            (Some(first), Some(last)) => last.t_ms - first.t_ms,
            _ => 0,
        }
    }

    pub fn rtt_ms(&self) -> f64 {
        self.history.back().map(|p| p.rtt_ms).unwrap_or(0.0)
    }

    pub fn rtt_floor_ms(&self) -> f64 {
        self.rtt_buckets
            .iter()
            .map(|(_, v)| *v)
            .reduce(f64::min)
            .unwrap_or(self.rtt_fast)
    }

    pub fn queue_floor_ms(&self) -> Option<f64> {
        self.queue_buckets.iter().map(|(_, v)| *v).reduce(f64::min)
    }

    /// RTT smoothed over ~200 ms; less sensitive to path noise than the
    /// latest sample.
    pub fn rtt_smoothed_ms(&self) -> f64 {
        self.rtt_fast
    }

    pub fn rtt_deviation_ms(&self) -> f64 {
        self.rtt_dev
    }

    /// RTT change over roughly the last 200 ms.
    pub fn rtt_trend_ms(&self) -> f64 {
        let Some(newest) = self.history.back() else {
            return 0.0;
        };
        self.history
            .iter()
            .rev()
            .find(|p| p.t_ms + TREND_SPAN_MS <= newest.t_ms)
            .map(|p| newest.rtt_ms - p.rtt_ms)
            .unwrap_or(0.0)
    }

    /// Sender-queue delay change over roughly the last 200 ms.
    pub fn queue_trend_ms(&self) -> f64 {
        let Some(newest) = self.history.back() else {
            return 0.0;
        };
        self.history
            .iter()
            .rev()
            .find(|p| p.t_ms + TREND_SPAN_MS <= newest.t_ms)
            .map(|p| newest.queue_ms - p.queue_ms)
            .unwrap_or(0.0)
    }

    /// Sum of unique bytes sent in (from_ms, to_ms] and the actual span the
    /// included samples cover.
    fn sent_between(
        &self,
        from_ms: u64,
        to_ms: u64,
    ) -> Option<(u64, u64, HistoryPoint, HistoryPoint)> {
        let base = *self.history.iter().rev().find(|p| p.t_ms <= from_ms)?;
        let end = *self.history.iter().rev().find(|p| p.t_ms <= to_ms)?;
        if end.t_ms <= base.t_ms {
            return None;
        }
        let sent = self
            .history
            .iter()
            .filter(|p| p.t_ms > base.t_ms && p.t_ms <= end.t_ms)
            .map(|p| p.sent_bytes)
            .sum();
        Some((sent, end.t_ms - base.t_ms, base, end))
    }

    pub fn delivery(&self, window_ms: u64, lag_ms: u64) -> Option<Delivery> {
        let newest = *self.history.back()?;
        let (sent, span, base, end) =
            self.sent_between(newest.t_ms.checked_sub(window_ms)?, newest.t_ms)?;
        // No traffic (or no byte counters) means no evidence either way.
        if span * 10 < window_ms * 6 || sent == 0 {
            return None;
        }
        let acked =
            sent as f64 - (f64::from(end.flight) - f64::from(base.flight)) * SRT_PAYLOAD_BYTES;
        let goodput_bps = (acked.max(0.0) * 8_000.0 / span as f64) as u64;
        let lagged_end = newest.t_ms.saturating_sub(lag_ms);
        let offered_bps = match lagged_end
            .checked_sub(window_ms)
            .and_then(|from| self.sent_between(from, lagged_end))
        {
            Some((sent, span, _, _)) if span * 10 >= window_ms * 6 => {
                (sent as f64 * 8_000.0 / span as f64) as u64
            }
            // Not enough history to compare like with like (the pipe may
            // still be filling); report no offered rate rather than a
            // misleading ratio.
            _ => 0,
        };
        Some(Delivery {
            goodput_bps,
            offered_bps,
        })
    }

    /// Fraction of packets sent in the window that were reported lost.
    pub fn loss_ratio(&self, window_ms: u64) -> f64 {
        let Some(newest) = self.history.back() else {
            return 0.0;
        };
        let (sent, lost) = self
            .history
            .iter()
            .filter(|p| p.t_ms + window_ms > newest.t_ms)
            .fold((0u64, 0u64), |(s, l), p| {
                (s + p.sent_bytes, l + u64::from(p.loss))
            });
        let packets = (sent as f64 / SRT_PAYLOAD_BYTES).max(1.0);
        lost as f64 / packets
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Thresholds {
    rtt_floor_ms: f64,
    increase_below_ms: f64,
    light_above_ms: f64,
    heavy_above_ms: f64,
    queue_light_ms: f64,
    queue_heavy_ms: f64,
    queue_severe_ms: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Signals {
    thresholds: Thresholds,
    delivery: Option<Delivery>,
    severe: bool,
    heavy: bool,
    light: bool,
    building: bool,
    stalled: bool,
    /// Elevated RTT/queue without any growth evidence, for a while.
    persistent: bool,
    /// RTT and queue back near their floors.
    drained: bool,
    healthy: bool,
}

#[derive(Clone, Copy, Debug)]
struct Episode {
    started_ms: u64,
    pre_target_bps: u64,
    pre_ceiling_bps: Option<u64>,
    stall: bool,
}

#[derive(Clone, Copy, Debug)]
struct BaselineCheck {
    until_ms: u64,
    rtt_ms: f64,
    floor_ms: f64,
    pre_target_bps: u64,
}

#[derive(Clone, Debug)]
pub struct AbrController {
    config: AbrConfig,
    internal_bitrate_bps: u64,
    last_srt_sample_ms: Option<u64>,
    estimator: Estimator,
    ceiling_bps: Option<u64>,
    /// When the ceiling was last raised from the capacity memory, and to
    /// what.
    restored_at_ms: Option<u64>,
    restored_to_bps: u64,
    restore_dwell_ms: u64,
    audio_bps: u64,
    episode: Option<Episode>,
    baseline_check: Option<BaselineCheck>,
    probe_permille: u64,
    probe_dwell_since_ms: Option<u64>,
    probe_dwell_ms: u64,
    next_increase_ms: u64,
    settle_until_ms: u64,
    increase_hold_until_ms: u64,
    elevated_since_ms: Option<u64>,
    last_control_state: &'static str,
    signals: Signals,
}

impl AbrController {
    pub fn new(config: AbrConfig) -> Self {
        let config = config.normalized();
        let internal_bitrate_bps = config.start_bps;
        Self {
            config,
            internal_bitrate_bps,
            last_srt_sample_ms: None,
            estimator: Estimator::default(),
            ceiling_bps: None,
            restored_at_ms: None,
            restored_to_bps: 0,
            restore_dwell_ms: RESTORE_DWELL_MS,
            audio_bps: 0,
            episode: None,
            baseline_check: None,
            probe_permille: PROBE_INITIAL_PERMILLE,
            probe_dwell_since_ms: None,
            probe_dwell_ms: PROBE_DWELL_MS,
            next_increase_ms: 0,
            settle_until_ms: 0,
            increase_hold_until_ms: 0,
            elevated_since_ms: None,
            last_control_state: STATE_WAITING,
            signals: Signals::default(),
        }
    }

    pub fn config(&self) -> &AbrConfig {
        &self.config
    }

    pub fn ceiling_bps(&self) -> Option<u64> {
        self.ceiling_bps
    }

    #[cfg(test)]
    pub fn debug_state(&self) -> String {
        format!(
            "int={} dwell={} rdwell={} since={:?} restored={:?} healthy={} hold_until={} settle_until={}",
            self.internal_bitrate_bps / 1000,
            self.probe_dwell_ms,
            self.restore_dwell_ms,
            self.probe_dwell_since_ms,
            self.restored_at_ms,
            self.signals.healthy,
            self.increase_hold_until_ms,
            self.settle_until_ms
        )
    }

    /// Highest recent delivery rate (all media), if any.
    pub fn capacity_memory_bps(&self) -> Option<u64> {
        self.estimator.delivery_memory_bps()
    }

    /// Change the live video ceiling without discarding learned RTT and queue
    /// baselines. A lower ceiling is reflected immediately.
    pub fn set_max_bps(&mut self, max_bps: u64) -> u64 {
        let max_bps = max_bps.max(self.config.min_bps).max(ROUNDING_BPS);
        self.config.max_bps = max_bps;
        self.internal_bitrate_bps = self.internal_bitrate_bps.min(max_bps);
        max_bps
    }

    /// Synchronize with a bitrate selected outside ABR, such as the value in
    /// use when automatic mode is enabled.
    pub fn set_current_bps(&mut self, current_bps: u64) -> u64 {
        let current_bps = current_bps.clamp(self.config.min_bps, self.config.max_bps);
        self.internal_bitrate_bps = current_bps;
        current_bps
    }

    /// Reset transient measurement state after a session transition. The
    /// remembered capacity ceiling is kept: it only shapes how quickly the
    /// controller ramps, and a stale value is corrected by the next
    /// congestion event or by probing above it.
    pub fn reset_for_bitrate(&mut self, current_bps: u64) {
        self.internal_bitrate_bps = current_bps.clamp(self.config.min_bps, self.config.max_bps);
        self.last_srt_sample_ms = None;
        self.estimator = Estimator::default();
        self.probe_dwell_since_ms = None;
        self.next_increase_ms = 0;
        self.settle_until_ms = 0;
        self.increase_hold_until_ms = 0;
        self.elevated_since_ms = None;
        self.episode = None;
        self.baseline_check = None;
        self.last_control_state = STATE_WAITING;
        self.signals = Signals::default();
    }

    /// Forget everything learned about the path (new output session).
    pub fn forget_capacity(&mut self) {
        self.ceiling_bps = None;
        self.probe_dwell_ms = PROBE_DWELL_MS;
        self.restored_at_ms = None;
        self.restore_dwell_ms = RESTORE_DWELL_MS;
        self.probe_permille = PROBE_INITIAL_PERMILLE;
    }

    /// Start closed-loop control from a startup probe result.
    pub(crate) fn seed_from_probe(
        &mut self,
        now_ms: u64,
        video_bps: u64,
        ceiling_video_bps: Option<u64>,
        estimator: &Estimator,
    ) {
        self.internal_bitrate_bps = video_bps.clamp(self.config.min_bps, self.config.max_bps);
        self.ceiling_bps = ceiling_video_bps.map(|c| c.max(self.config.min_bps));
        self.restored_at_ms = None;
        self.restore_dwell_ms = RESTORE_DWELL_MS;
        self.probe_permille = PROBE_INITIAL_PERMILLE;
        self.probe_dwell_since_ms = None;
        self.probe_dwell_ms = PROBE_DWELL_MS;
        self.estimator = estimator.carry_baselines();
        self.last_srt_sample_ms = None;
        self.next_increase_ms = 0;
        self.settle_until_ms = 0;
        self.increase_hold_until_ms = now_ms.saturating_add(STARTUP_HOLD_MS);
        self.elevated_since_ms = None;
        self.episode = None;
        self.baseline_check = None;
    }

    pub fn decide(&mut self, sample: &AbrSample) -> AbrDecision {
        let link_capacity_bps = sample
            .links
            .iter()
            .filter(|link| link.enabled && link.payload_eligible && link.capacity_ready)
            .map(|link| link.target_bps)
            .sum();
        let current = sample
            .current_video_bps
            .clamp(self.config.min_bps, self.config.max_bps);

        if sample.all_links_down {
            self.internal_bitrate_bps = self.config.min_bps;
            self.last_control_state = STATE_DISCONNECTED;
            let applied_bps = rounded_bps(self.internal_bitrate_bps)
                .clamp(self.config.min_bps, self.config.max_bps);
            return self.decision(sample, link_capacity_bps, current, applied_bps, true, true);
        }

        // Never replay one interval's congestion evidence. The plugin can
        // miss a sample while the socket lock is busy; holding is safer than
        // applying the same reduction repeatedly.
        if !sample.srt.ready
            || sample.srt.rtt_ms == 0
            || sample.srt.latency_ms == 0
            || self.last_srt_sample_ms == Some(sample.srt.sampled_at_ms)
            || self
                .last_srt_sample_ms
                .is_some_and(|last| sample.srt.sampled_at_ms < last)
        {
            if !sample.srt.ready || sample.srt.rtt_ms == 0 || sample.srt.latency_ms == 0 {
                self.last_control_state = STATE_WAITING;
            }
            return self.decision(sample, link_capacity_bps, current, current, false, false);
        }
        self.last_srt_sample_ms = Some(sample.srt.sampled_at_ms);
        self.audio_bps = sample.audio_bps;
        self.estimator.push(sample.now_ms, &sample.srt);
        let signals = self.classify(sample);
        self.signals = signals;

        let now = sample.now_ms;
        let before = self.internal_bitrate_bps;
        let video_goodput = signals
            .delivery
            .map(|d| d.goodput_bps.saturating_sub(sample.audio_bps));
        // Target sized from the measured delivery rate, minus what is needed
        // to drain the standing queue (RTT above the floor) within
        // DRAIN_TIME_MS, but never below `floor_factor` of the delivery rate.
        let excess_s =
            (f64::from(sample.srt.rtt_ms) - signals.thresholds.rtt_floor_ms).max(0.0) / 1_000.0;
        let delivery_target = |factor: f64, floor_factor: f64| {
            signals.delivery.zip(video_goodput).map(|(d, video)| {
                let drain = d.goodput_bps as f64 * excess_s * 1_000.0 / DRAIN_TIME_MS as f64;
                (video as f64 * factor - drain).max(video as f64 * floor_factor) as u64
            })
        };
        let escalate = |target: u64| target < before * ESCALATE_BELOW_PERMILLE / 1_000;

        // Resolve a pending baseline check: if the cut did not shrink the
        // RTT excess, the path's base RTT moved (handover, satellite
        // switch). Adopt it as the new floor and undo the recent cuts.
        if let Some(check) = self.baseline_check
            && now >= check.until_ms
        {
            self.baseline_check = None;
            let excess_then = check.rtt_ms - check.floor_ms;
            let excess_now = self.estimator.rtt_smoothed_ms() - check.floor_ms;
            if excess_then > 0.0 && excess_now > excess_then * BASELINE_SHIFT_KEEP {
                self.estimator
                    .rebase(now, f64::from(sample.srt.send_buffer_ms));
                let mut restore = check.pre_target_bps;
                if let Some(episode) = self.episode
                    && now < episode.started_ms + BASELINE_UNDO_WINDOW_MS
                {
                    restore = restore.max(episode.pre_target_bps);
                    self.ceiling_bps = episode.pre_ceiling_bps.or(self.ceiling_bps);
                }
                self.internal_bitrate_bps = self.internal_bitrate_bps.max(restore);
                self.episode = None;
                self.settle_until_ms = now;
                self.increase_hold_until_ms = now;
                self.elevated_since_ms = None;
                self.signals = self.classify(sample);
            }
        }
        let signals = self.signals;
        let before = self.internal_bitrate_bps;

        // A short stall that has already drained: return to where we were.
        if let Some(episode) = self.episode
            && episode.stall
            && signals.drained
            && !signals.stalled
            && now < episode.started_ms + STALL_RESTORE_WINDOW_MS
        {
            self.episode = None;
            self.ceiling_bps = episode.pre_ceiling_bps;
            let restore = episode.pre_target_bps * STALL_RESTORE_PERMILLE / 1_000;
            self.internal_bitrate_bps = self.internal_bitrate_bps.max(restore);
            self.settle_until_ms = now;
            self.increase_hold_until_ms = now.saturating_add(INCREASE_HOLD_MIN_MS / 2);
            self.last_control_state = STATE_RESTORED;
            let applied_bps = rounded_bps(
                self.internal_bitrate_bps
                    .clamp(self.config.min_bps, self.config.max_bps),
            )
            .clamp(self.config.min_bps, self.config.max_bps);
            return self.decision(sample, link_capacity_bps, current, applied_bps, false, true);
        }

        let settling = now < self.settle_until_ms;
        let mut emergency = false;
        let mut stressed = false;
        if signals.severe || signals.heavy || signals.light {
            self.baseline_check = None;
        }
        if signals.severe && before > self.config.min_bps {
            stressed = true;
            let by_delivery = delivery_target(SEVERE_DELIVERY_FACTOR, SEVERE_DELIVERY_FLOOR);
            let target = by_delivery.map_or(before / 2, |t| t.min(before * 9 / 10));
            if !settling || by_delivery.is_some_and(escalate) {
                self.decrease(now, target, video_goodput, signals.stalled);
                self.last_control_state = if signals.stalled {
                    STATE_STALL
                } else {
                    STATE_SEVERE_CONGESTION
                };
                emergency = true;
            } else {
                self.last_control_state = STATE_SETTLING;
            }
        } else if signals.heavy {
            stressed = true;
            let by_delivery = delivery_target(HEAVY_DELIVERY_FACTOR, HEAVY_DELIVERY_FLOOR);
            let target = before
                .saturating_sub(DECREASE_MIN_BPS.max(before * HEAVY_DECREASE_PERMILLE / 1_000))
                .min(by_delivery.unwrap_or(u64::MAX));
            if !settling || by_delivery.is_some_and(escalate) {
                self.decrease(now, target, video_goodput, signals.stalled);
                self.last_control_state = if signals.stalled {
                    STATE_STALL
                } else {
                    STATE_HEAVY_CONGESTION
                };
            } else {
                self.last_control_state = STATE_SETTLING;
            }
        } else if signals.light {
            stressed = true;
            let target = before
                .saturating_sub(DECREASE_MIN_BPS.max(before * LIGHT_DECREASE_PERMILLE / 1_000))
                .min(
                    delivery_target(LIGHT_DELIVERY_FACTOR, HEAVY_DELIVERY_FLOOR)
                        .unwrap_or(u64::MAX),
                );
            if !settling {
                // Light congestion means the knee is at or below the current
                // target; never raise the ceiling estimate above it.
                self.decrease(
                    now,
                    target,
                    video_goodput.map(|g| g.min(before)),
                    signals.stalled,
                );
                self.last_control_state = STATE_LIGHT_CONGESTION;
            } else {
                self.last_control_state = STATE_SETTLING;
            }
        } else if signals.persistent && self.baseline_check.is_none() && !settling {
            // Elevated but not growing: either a standing queue or a new
            // base RTT. Cut enough to drain a queue and watch the RTT.
            stressed = true;
            let target = before
                .saturating_sub(DECREASE_MIN_BPS.max(before * HEAVY_DECREASE_PERMILLE / 1_000))
                .min(
                    delivery_target(LIGHT_DELIVERY_FACTOR, HEAVY_DELIVERY_FLOOR)
                        .unwrap_or(u64::MAX),
                );
            let rtt_ms = self.estimator.rtt_smoothed_ms();
            let floor_ms = signals.thresholds.rtt_floor_ms;
            // No ceiling update: this is a measurement, not congestion. Keep
            // the episode that may need undoing if this is a base shift.
            let episode = self.episode;
            self.decrease(now, target, None, true);
            if let Some(episode) = episode
                && now < episode.started_ms + BASELINE_UNDO_WINDOW_MS
            {
                self.episode = Some(episode);
            }
            self.baseline_check = Some(BaselineCheck {
                until_ms: now.saturating_add(BASELINE_CHECK_MS.max(self.settle_until_ms - now)),
                rtt_ms,
                floor_ms,
                pre_target_bps: before,
            });
            self.last_control_state = STATE_BASELINE_CHECK;
        } else if self.baseline_check.is_some() {
            self.last_control_state = STATE_BASELINE_CHECK;
        } else if settling || now < self.increase_hold_until_ms {
            self.last_control_state = STATE_SETTLING;
        } else if signals.healthy {
            self.increase(now);
        } else {
            self.last_control_state = STATE_HOLD;
        }

        self.internal_bitrate_bps = self
            .internal_bitrate_bps
            .clamp(self.config.min_bps, self.config.max_bps);
        let applied_bps =
            rounded_bps(self.internal_bitrate_bps).clamp(self.config.min_bps, self.config.max_bps);
        self.decision(
            sample,
            link_capacity_bps,
            current,
            applied_bps,
            emergency,
            stressed,
        )
    }

    fn classify(&mut self, sample: &AbrSample) -> Signals {
        let srt = &sample.srt;
        let est = &self.estimator;
        let rtt = f64::from(srt.rtt_ms);
        let latency = f64::from(srt.latency_ms);
        let floor = est.rtt_floor_ms();
        let dev = est.rtt_deviation_ms();
        let queue = f64::from(srt.send_buffer_ms);
        let queue_floor = est.queue_floor_ms().unwrap_or(queue);

        let increase_below_ms = floor + 10.0_f64.max(floor * 0.10).max(dev * 2.0);
        let light_above_ms = floor + 25.0_f64.max(floor * 0.25).max(dev * 4.0);
        let heavy_above_ms = floor + 80.0_f64.max(floor * 0.75).max(dev * 8.0);
        // Severe needs a third of the latency budget and clear inflation over
        // the floor; half the budget is severe even if the floor itself was
        // learned while congested.
        let severe_rtt_ms = (latency / 3.0).max(heavy_above_ms.min(latency / 2.0));
        let queue_light_ms = queue_floor + 40.0_f64.max(floor * 0.3).max(dev * 4.0);
        let queue_heavy_ms = queue_floor + 120.0_f64.max(floor * 0.8).max(latency / 10.0);
        let queue_severe_ms = (latency / 2.0).max(queue_heavy_ms + 1.0);

        let delivery = est.delivery(DELIVERY_WINDOW_MS, floor as u64);
        let ratio = delivery.map(|d| d.ratio()).unwrap_or(1.0);
        let trend = est.rtt_trend_ms();
        let rising =
            trend > 4.0_f64.max(dev * 1.5) || est.queue_trend_ms() > 10.0_f64.max(dev * 3.0);
        let building = ratio < 0.92;
        let elevated = rtt > light_above_ms || queue > queue_light_ms;
        // Increases blocked by RTT/queue that sit above the increase
        // threshold without growing.
        let blocked = est.rtt_smoothed_ms() > increase_below_ms || queue > queue_light_ms;

        let now = sample.now_ms;
        let elevated_for = if blocked && !rising && !building {
            let since = *self.elevated_since_ms.get_or_insert(now);
            now - since
        } else {
            self.elevated_since_ms = None;
            0
        };

        // RTT or queue above the floor only means congestion when something
        // is growing; a flat elevation may be a new base RTT after a
        // handover and is resolved by a baseline check instead.
        let growth = rising || building;
        let stalled = est
            .delivery(STALL_WINDOW_MS, floor as u64)
            .is_some_and(|d| d.offered_bps > 0 && d.ratio() < STALL_RATIO);
        let severe = rtt >= severe_rtt_ms || queue >= queue_severe_ms;
        let heavy = ((rtt > heavy_above_ms
            || (rtt > latency / 5.0 && rtt > light_above_ms)
            || queue > queue_heavy_ms)
            && growth)
            || (ratio < 0.75 && rtt > light_above_ms);
        let light = elevated && growth;
        let persistent = !growth && elevated_for >= ELEVATED_PERSIST_MS;
        let drained = est.rtt_smoothed_ms() <= light_above_ms && queue <= queue_light_ms && !rising;
        let healthy = est.history_span_ms() >= WARMUP_MS
            && est.rtt_smoothed_ms().min(rtt) <= increase_below_ms
            && queue <= queue_light_ms
            && !rising
            && ratio >= 0.95;

        Signals {
            thresholds: Thresholds {
                rtt_floor_ms: floor,
                increase_below_ms,
                light_above_ms,
                heavy_above_ms,
                queue_light_ms,
                queue_heavy_ms,
                queue_severe_ms,
            },
            delivery,
            severe,
            heavy,
            light,
            building,
            stalled,
            persistent,
            drained,
            healthy,
        }
    }

    /// `stall`: the evidence is a stall or a measurement, so the ceiling is
    /// left alone.
    fn decrease(&mut self, now: u64, target: u64, video_goodput: Option<u64>, stall: bool) {
        let before = self.internal_bitrate_bps;
        let target = target.clamp(self.config.min_bps, before.max(self.config.min_bps));
        // A decrease after the previous one's hold has expired starts a new
        // congestion episode.
        if now >= self.increase_hold_until_ms || self.episode.is_none() {
            self.episode = Some(Episode {
                started_ms: now,
                pre_target_bps: before,
                pre_ceiling_bps: self.ceiling_bps,
                stall: false,
            });
        }
        if stall && let Some(episode) = self.episode.as_mut() {
            episode.stall = true;
        }
        if target < before && !stall {
            // The delivery rate measured while congested is the capacity;
            // the current target may already have been cut below it.
            let congestion_point = video_goodput.unwrap_or(before).max(self.config.min_bps);
            if let Some(at) = self.restored_at_ms {
                self.restored_at_ms = None;
                if now < at + RESTORE_GRACE_MS
                    && congestion_point
                        < self.restored_to_bps * RESTORE_FAILED_BELOW_PERMILLE / 1_000
                {
                    // The capacity memory was optimistic: wait longer next time.
                    self.restore_dwell_ms = (self.restore_dwell_ms * 2).min(RESTORE_DWELL_MAX_MS);
                } else {
                    self.restore_dwell_ms = RESTORE_DWELL_MS;
                }
            }
            let confirmed = self
                .ceiling_bps
                .is_some_and(|c| congestion_point.abs_diff(c) <= c * SAME_CEILING_PERMILLE / 1_000);
            self.probe_dwell_ms = if confirmed {
                (self.probe_dwell_ms * 2).min(PROBE_DWELL_MAX_MS)
            } else {
                PROBE_DWELL_MS
            };
            self.ceiling_bps = Some(match self.ceiling_bps {
                Some(ceiling) if congestion_point >= ceiling => {
                    (ceiling * 3 + congestion_point) / 4
                }
                _ => congestion_point,
            });
        }
        self.internal_bitrate_bps = target;
        let rtt = self.estimator.rtt_ms() as u64;
        let settle = rtt
            .saturating_add(ENCODER_LAG_MS)
            .clamp(SETTLE_MIN_MS, SETTLE_MAX_MS);
        self.settle_until_ms = now.saturating_add(settle);
        self.increase_hold_until_ms = now.saturating_add(INCREASE_HOLD_MIN_MS.max(settle));
        self.probe_permille = PROBE_INITIAL_PERMILLE;
        self.probe_dwell_since_ms = None;
    }

    fn increase(&mut self, now: u64) {
        let internal = self.internal_bitrate_bps;
        if internal >= self.config.max_bps {
            self.last_control_state = STATE_HOLD;
            return;
        }
        if now < self.next_increase_ms {
            self.last_control_state = if self.last_control_state == STATE_PROBING {
                STATE_PROBING
            } else {
                STATE_INCREASING
            };
            return;
        }
        match self.ceiling_bps {
            Some(ceiling) if internal >= ceiling * APPROACH_ZONE_PERMILLE / 1_000 => {
                let dwell_since = *self.probe_dwell_since_ms.get_or_insert(now);
                if self
                    .restored_at_ms
                    .is_some_and(|at| now >= at + RESTORE_GRACE_MS)
                    || (self.restored_at_ms.is_some()
                        && internal >= self.restored_to_bps * FAST_ZONE_PERMILLE / 1_000)
                {
                    // The retried capacity held: retry promptly next time.
                    self.restored_at_ms = None;
                    self.restore_dwell_ms = RESTORE_DWELL_MS;
                }
                let memory = self
                    .estimator
                    .delivery_memory_bps()
                    .map(|m| m.saturating_sub(self.audio_bps));
                if let Some(memory) = memory
                    && memory > ceiling * MEMORY_RETRY_ABOVE_PERMILLE / 1_000
                    && now >= dwell_since + self.restore_dwell_ms
                {
                    // Recently the path delivered far more than this ceiling
                    // (typical after a fade on a moving link): retry higher
                    // with the fast ramp instead of creeping up.
                    let retry = memory.min(ceiling * MEMORY_RETRY_FACTOR);
                    self.restored_at_ms = Some(now);
                    self.restored_to_bps = retry;
                    self.ceiling_bps = Some(retry);
                    self.probe_dwell_since_ms = None;
                    self.last_control_state = STATE_INCREASING;
                    return;
                }
                if now < dwell_since + self.probe_dwell_ms {
                    self.last_control_state = STATE_HOLD;
                    return;
                }
                let step = SMALL_INCREASE_MIN_BPS.max(internal * self.probe_permille / 1_000);
                self.internal_bitrate_bps = internal.saturating_add(step);
                if self.probe_permille >= PROBE_MAX_PERMILLE {
                    // Several successful probes in a row: the old ceiling no
                    // longer describes the path. Ramp until congestion says
                    // where the new limit is.
                    self.ceiling_bps = None;
                    self.probe_permille = PROBE_INITIAL_PERMILLE;
                } else {
                    self.ceiling_bps = Some(ceiling.max(self.internal_bitrate_bps));
                    self.probe_permille = (self.probe_permille * 2).min(PROBE_MAX_PERMILLE);
                }
                self.next_increase_ms = now.saturating_add(PROBE_INTERVAL_MS);
                self.last_control_state = STATE_PROBING;
            }
            Some(ceiling) if internal >= ceiling * FAST_ZONE_PERMILLE / 1_000 => {
                self.probe_dwell_since_ms = None;
                let step =
                    SMALL_INCREASE_MIN_BPS.max(internal * APPROACH_INCREASE_PERMILLE / 1_000);
                self.internal_bitrate_bps =
                    internal.saturating_add(step).min(ceiling.max(internal));
                self.next_increase_ms = now.saturating_add(APPROACH_INCREASE_INTERVAL_MS);
                self.last_control_state = STATE_INCREASING;
            }
            ceiling => {
                self.probe_dwell_since_ms = None;
                let mut step = FAST_INCREASE_MIN_BPS.max(internal * FAST_INCREASE_PERMILLE / 1_000);
                let mut next = internal.saturating_add(step);
                if let Some(ceiling) = ceiling {
                    let zone_end = ceiling * FAST_ZONE_PERMILLE / 1_000;
                    step = step.max(zone_end.saturating_sub(internal) / FAST_GAP_DIVISOR);
                    next = internal
                        .saturating_add(step)
                        .min((ceiling * FAST_ZONE_CAP_PERMILLE / 1_000).max(internal));
                }
                self.internal_bitrate_bps = next;
                self.next_increase_ms = now.saturating_add(FAST_INCREASE_INTERVAL_MS);
                self.last_control_state = STATE_INCREASING;
            }
        }
    }

    fn decision(
        &self,
        sample: &AbrSample,
        link_capacity_bps: u64,
        current_bps: u64,
        applied_bps: u64,
        emergency: bool,
        stressed: bool,
    ) -> AbrDecision {
        let t = self.signals.thresholds;
        AbrDecision {
            link_capacity_bps,
            srt_capacity_bps: self.signals.delivery.map_or(0, |d| d.goodput_bps),
            estimated_capacity_bps: self.ceiling_bps.unwrap_or(0),
            media_budget_bps: self.internal_bitrate_bps.saturating_add(sample.audio_bps),
            recommended_bps: applied_bps,
            applied_bps,
            changed: applied_bps != current_bps,
            emergency,
            srt_limited: self.signals.building,
            transport_stressed: stressed,
            control_state: self.last_control_state.to_string(),
            rtt_floor_ms: to_u32(t.rtt_floor_ms),
            rtt_increase_below_ms: to_u32(t.increase_below_ms),
            rtt_decrease_above_ms: to_u32(t.light_above_ms),
            rtt_heavy_above_ms: to_u32(t.heavy_above_ms),
            queue_light_ms: to_u32(t.queue_light_ms),
            queue_heavy_ms: to_u32(t.queue_heavy_ms),
            queue_severe_ms: to_u32(t.queue_severe_ms),
        }
    }
}

pub(crate) fn to_u32(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else if value >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        value as u32
    }
}

pub(crate) fn rounded_bps(value: u64) -> u64 {
    value / ROUNDING_BPS * ROUNDING_BPS
}

#[cfg(test)]
#[path = "abr_sim.rs"]
pub(crate) mod sim;

#[cfg(test)]
mod tests {
    use super::sim::{self, Sim, SimConfig};
    use super::*;

    fn controller(start_bps: u64) -> AbrController {
        AbrController::new(AbrConfig {
            min_bps: 500_000,
            start_bps,
            max_bps: 20_000_000,
        })
    }

    fn sample(now_ms: u64, current_bps: u64, rtt_ms: u32) -> AbrSample {
        AbrSample {
            links: vec![LinkCapacity {
                enabled: true,
                payload_eligible: true,
                capacity_ready: true,
                target_bps: 10_000_000,
                delivered_bps: current_bps,
            }],
            audio_bps: 128_000,
            current_video_bps: current_bps,
            now_ms,
            capacity_ready: true,
            all_links_down: false,
            srt: SrtCapacity {
                ready: true,
                sampled_at_ms: now_ms,
                bandwidth_bps: 100_000_000,
                send_rate_bps: current_bps + 128_000,
                send_buffer_ms: rtt_ms + 10,
                send_buffer_packets: 0,
                rtt_ms,
                latency_ms: 2_000,
                retransmit_permille: 0,
                dropped_bytes: 0,
                sent_unique_bytes: (current_bps + 128_000) / 8 / 50,
                packets_in_flight: 20,
                sender_loss_packets: 0,
            },
        }
    }

    /// Feed `count` healthy 50 ms samples without allowing increases, so the
    /// controller learns its RTT and queue floors. Returns the next time.
    fn warm(abr: &mut AbrController, current: u64, count: u64) -> u64 {
        let max = abr.config().max_bps;
        abr.set_max_bps(current);
        let mut now = 0;
        for _ in 0..count {
            now += 20;
            abr.decide(&sample(now, current, 50));
        }
        abr.set_max_bps(max);
        now + 20
    }

    #[test]
    fn capacity_estimates_do_not_set_the_encoder_target() {
        let mut abr = controller(2_000_000);
        let mut input = sample(20, 2_000_000, 50);
        input.links[0].target_bps = 100_000_000;
        input.srt.bandwidth_bps = 200_000_000;

        let decision = abr.decide(&input);

        assert_eq!(decision.link_capacity_bps, 100_000_000);
        assert_eq!(decision.applied_bps, 2_000_000);
    }

    #[test]
    fn severe_rtt_congestion_cuts_hard_without_dropping_to_the_floor() {
        let mut abr = controller(6_000_000);
        let now = warm(&mut abr, 6_000_000, 25);
        let decision = abr.decide(&sample(now, 6_000_000, 700));

        assert!(decision.emergency);
        assert_eq!(decision.control_state, STATE_SEVERE_CONGESTION);
        assert!(decision.applied_bps <= 5_400_000);
        assert!(decision.applied_bps > 500_000);
    }

    #[test]
    fn heavy_rtt_congestion_cuts_at_least_ten_percent() {
        let mut abr = controller(6_000_000);
        let now = warm(&mut abr, 6_000_000, 25);
        let decision = abr.decide(&sample(now, 6_000_000, 200));

        assert_eq!(decision.control_state, STATE_HEAVY_CONGESTION);
        assert!(decision.transport_stressed);
        assert!(decision.applied_bps <= 5_400_000);
    }

    #[test]
    fn sender_queue_growth_drives_congestion_without_rtt_growth() {
        let mut abr = controller(6_000_000);
        let now = warm(&mut abr, 6_000_000, 25);
        let mut queued = sample(now, 6_000_000, 50);
        queued.srt.bandwidth_bps = 0;
        queued.srt.send_buffer_ms = 400;

        let decision = abr.decide(&queued);

        assert_eq!(decision.control_state, STATE_HEAVY_CONGESTION);
        assert!(decision.applied_bps < 6_000_000);
    }

    #[test]
    fn decreases_wait_for_the_previous_cut_to_take_effect() {
        let mut abr = controller(6_000_000);
        let now = warm(&mut abr, 6_000_000, 25);
        let first = abr.decide(&sample(now, 6_000_000, 200));
        let held = abr.decide(&sample(now + 20, first.applied_bps, 220));
        // Still rising once the settle time is over: cut again.
        let later = abr.decide(&sample(now + 2_000, held.applied_bps, 300));

        assert_eq!(first.control_state, STATE_HEAVY_CONGESTION);
        assert_eq!(held.applied_bps, first.applied_bps);
        assert_eq!(held.control_state, STATE_SETTLING);
        assert!(later.applied_bps < held.applied_bps);
    }

    #[test]
    fn a_base_rtt_shift_is_adopted_and_the_cut_undone() {
        // Handover: base RTT steps from 50 to 140 ms and stays there, with
        // delivery keeping up. One cut on the step is fine; the controller
        // must then recognise the new baseline and return to the old rate.
        let mut abr = controller(6_000_000);
        let mut now = warm(&mut abr, 6_000_000, 50);
        let mut current = 6_000_000;
        let mut lowest = current;
        for _ in 0..300 {
            now += 20;
            let d = abr.decide(&sample(now, current, 140));
            current = d.applied_bps;
            lowest = lowest.min(current);
        }
        let d = abr.decide(&sample(now + 20, current, 140));
        assert!(d.rtt_floor_ms >= 130, "floor {}", d.rtt_floor_ms);
        assert!(current >= 5_700_000, "stayed cut at {current}");
        assert!(lowest >= 4_000_000, "walked down to {lowest}");
    }

    #[test]
    fn a_short_stall_restores_the_previous_rate_and_ceiling() {
        // LEO-style handover: ~25 Mb/s path, 200 ms outage.
        let cfg = SimConfig {
            capacity: vec![(0, 25_000_000), (20_000, 50_000), (20_200, 25_000_000)],
            buffer_ms: 300.0,
            max_bps: 15_000_000,
            ..SimConfig::default()
        };
        let mut sim = Sim::new(cfg.clone());
        let mut abr = sim::controller_for(&cfg, 15_000_000);
        let trace = sim::run_abr(&mut abr, &mut sim, 15_000_000, 24_000);
        let back = sim::time_to_reach(&trace, 20_200, 13_000_000).expect("restored");
        assert!(back <= 2_500, "took {back} ms to return after the stall");
        if let Some(ceiling) = abr.ceiling_bps() {
            assert!(
                ceiling >= 12_000_000,
                "learned the stall as capacity: {ceiling}"
            );
        }
    }

    #[test]
    fn mobile_and_satellite_paths_meet_targets() {
        let scenarios = sim::mobility_scenarios();
        let floors = [
            // (efficiency, glitch ms) per scenario, with margin below the
            // measured values: guards against regressions, not tuning goals.
            (0.50, 300),
            (0.38, 300),
            (0.40, 1_500),
            (0.62, 300),
            (0.75, 0),
            (1.00, 0),
        ];
        for ((name, cfg), (efficiency, glitch_ms)) in scenarios.into_iter().zip(floors) {
            let mut sim = Sim::new(cfg.clone());
            let mut abr = sim::controller_for(&cfg, 4_000_000);
            let trace = sim::run_abr(&mut abr, &mut sim, 4_000_000, 180_000);
            let m = sim::mobility_metrics(&cfg, &trace, 10_000);
            assert!(
                m.efficiency >= efficiency,
                "{name}: efficiency {:.2}",
                m.efficiency
            );
            assert!(
                m.glitch_ms <= glitch_ms,
                "{name}: glitches {} ms",
                m.glitch_ms
            );
            assert!(
                m.mean_rtt_ms < 200.0,
                "{name}: mean RTT {:.0}",
                m.mean_rtt_ms
            );
        }
    }

    #[test]
    fn increases_are_held_after_a_decrease() {
        let mut abr = controller(6_000_000);
        let mut now = warm(&mut abr, 6_000_000, 25);
        let cut = abr.decide(&sample(now, 6_000_000, 200));
        let mut current = cut.applied_bps;
        // Healthy again right away: no increase during the hold.
        for _ in 0..40 {
            now += 20;
            let d = abr.decide(&sample(now, current, 50));
            assert!(d.applied_bps <= current, "increased during hold at {now}");
            current = d.applied_bps;
        }
        for _ in 0..100 {
            now += 20;
            current = abr.decide(&sample(now, current, 50)).applied_bps;
        }
        assert!(current > cut.applied_bps);
    }

    #[test]
    fn healthy_feedback_ramps_quickly_without_a_ceiling() {
        let mut abr = controller(1_500_000);
        let mut current = 1_500_000;

        for now_ms in (20..=2_000).step_by(20) {
            current = abr.decide(&sample(now_ms, current, 50)).applied_bps;
        }

        // ~10 % per 250 ms: well above +30 kb/s + 1/30 per 500 ms.
        assert!(current >= 2_800_000, "only reached {current}");
        assert_eq!(current % ROUNDING_BPS, 0);
    }

    #[test]
    fn repeated_srt_sample_is_not_applied_twice() {
        let mut abr = controller(6_000_000);
        let now = warm(&mut abr, 6_000_000, 25);
        let first_sample = sample(now, 6_000_000, 200);
        let first = abr.decide(&first_sample);

        let mut repeated = first_sample;
        repeated.now_ms = now + 3_000;
        repeated.current_video_bps = first.applied_bps;
        let held = abr.decide(&repeated);

        assert_eq!(held.applied_bps, first.applied_bps);
        assert!(!held.changed);
    }

    #[test]
    fn all_links_down_uses_the_floor() {
        let mut abr = controller(5_000_000);
        let mut input = sample(20, 5_000_000, 50);
        input.all_links_down = true;

        let decision = abr.decide(&input);

        assert_eq!(decision.applied_bps, 500_000);
        assert_eq!(decision.control_state, STATE_DISCONNECTED);
    }

    #[test]
    fn missing_feedback_holds_the_current_bitrate() {
        let mut abr = controller(5_000_000);
        let mut input = sample(20, 5_000_000, 0);
        input.srt.ready = false;

        let decision = abr.decide(&input);

        assert_eq!(decision.applied_bps, 5_000_000);
        assert_eq!(decision.control_state, STATE_WAITING);
    }

    #[test]
    fn lowering_maximum_clamps_the_internal_target() {
        let mut abr = controller(8_000_000);
        assert_eq!(abr.set_max_bps(3_000_000), 3_000_000);
        let mut input = sample(20, 3_000_000, 0);
        input.srt.ready = false;

        assert_eq!(abr.decide(&input).applied_bps, 3_000_000);
    }

    #[test]
    fn external_bitrate_sync_preserves_subsequent_control() {
        let mut abr = controller(1_500_000);
        let now = warm(&mut abr, 1_500_000, 25);
        assert_eq!(abr.set_current_bps(4_000_000), 4_000_000);

        let decision = abr.decide(&sample(now, 4_000_000, 200));

        // The cut is sized from what the path actually delivered while the
        // previous (lower) bitrate was in use.
        assert!(decision.applied_bps < 4_000_000);
        assert!(decision.applied_bps > 500_000);
        assert_eq!(decision.control_state, STATE_HEAVY_CONGESTION);
    }

    #[test]
    fn delivery_rate_sizes_the_cut() {
        // 8 Mb/s offered into a 3 Mb/s path: one settled episode should land
        // near the measured capacity instead of stepping down or hitting the
        // floor.
        let cfg = SimConfig {
            capacity: vec![(0, 3_000_000)],
            ..SimConfig::default()
        };
        let mut sim = Sim::new(cfg.clone());
        let mut abr = sim::controller_for(&cfg, 8_000_000);
        let trace = sim::run_abr(&mut abr, &mut sim, 8_000_000, 10_000);
        let ceiling = abr.ceiling_bps().expect("congestion learned");
        assert!(
            (2_400_000..=3_200_000).contains(&ceiling),
            "ceiling {ceiling}"
        );
        let settled = sim::metrics(&trace, 5_000, 10_000, cfg.audio_bps);
        assert!(
            settled.min_target_bps >= 1_800_000,
            "{}",
            settled.min_target_bps
        );
        assert!(settled.utilization > 0.75, "{}", settled.utilization);
    }

    #[test]
    fn simulated_paths_meet_agility_and_stability_targets() {
        // Steady path: few reversals, low queueing.
        let cfg = SimConfig::default();
        let mut sim = Sim::new(cfg.clone());
        let mut abr = sim::controller_for(&cfg, 6_000_000);
        let trace = sim::run_abr(&mut abr, &mut sim, 6_000_000, 60_000);
        let m = sim::metrics(&trace, 20_000, 60_000, cfg.audio_bps);
        assert!(m.reversals <= 8, "steady reversals {}", m.reversals);
        assert!(
            m.utilization >= 0.90,
            "steady utilization {}",
            m.utilization
        );
        assert!(m.mean_rtt_ms < 80.0, "steady RTT {}", m.mean_rtt_ms);

        // Capacity dip: no collapse to the floor, quick recovery.
        let cfg = SimConfig {
            capacity: vec![(0, 8_000_000), (20_000, 3_000_000), (35_000, 8_000_000)],
            ..SimConfig::default()
        };
        let mut sim = Sim::new(cfg.clone());
        let mut abr = sim::controller_for(&cfg, 6_000_000);
        let trace = sim::run_abr(&mut abr, &mut sim, 6_000_000, 70_000);
        let m = sim::metrics(&trace, 22_000, 35_000, cfg.audio_bps);
        assert!(m.min_target_bps > 500_000, "dip hit the floor");
        assert!(m.utilization >= 0.75, "dip utilization {}", m.utilization);
        let recovery = sim::time_to_reach(&trace, 35_000, 6_000_000).expect("recovered");
        assert!(recovery <= 10_000, "dip recovery {recovery} ms");

        // Recovery from the floor.
        let cfg = SimConfig {
            capacity: vec![(0, 12_000_000)],
            max_bps: 8_000_000,
            ..SimConfig::default()
        };
        let mut sim = Sim::new(cfg.clone());
        let mut abr = sim::controller_for(&cfg, 500_000);
        let trace = sim::run_abr(&mut abr, &mut sim, 500_000, 20_000);
        let ramp = sim::time_to_reach(&trace, 0, 7_000_000).expect("ramped");
        assert!(ramp <= 9_000, "floor ramp {ramp} ms");

        // Noisy path: stays close to capacity without sawing.
        let cfg = SimConfig {
            capacity: vec![(0, 5_000_000)],
            base_rtt_ms: 120.0,
            rtt_noise_ms: 30.0,
            buffer_ms: 2_000.0,
            ..SimConfig::default()
        };
        let mut sim = Sim::new(cfg.clone());
        let mut abr = sim::controller_for(&cfg, 6_000_000);
        let trace = sim::run_abr(&mut abr, &mut sim, 6_000_000, 60_000);
        let m = sim::metrics(&trace, 20_000, 60_000, cfg.audio_bps);
        assert!(m.reversals <= 12, "noisy reversals {}", m.reversals);
        assert!(m.utilization >= 0.85, "noisy utilization {}", m.utilization);
        assert!(
            m.min_target_bps >= 3_500_000,
            "noisy dipped to {}",
            m.min_target_bps
        );
    }
}
