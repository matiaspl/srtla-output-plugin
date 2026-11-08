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
/// A ceiling drop below this fraction of the previous one is a collapse; the
/// previous ceiling is remembered and retried once the path looks healthy.
const COLLAPSE_PERMILLE: u64 = 700;
const RECOVERY_DWELL_MS: u64 = 1_500;
const RECOVERY_MEMORY_MS: u64 = 60_000;
/// A collapse this soon after a restore means the restore failed; do not
/// keep retrying the old capacity.
const RESTORE_GRACE_MS: u64 = 10_000;
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
/// Feedback needed after a reset before the first increase.
const WARMUP_MS: u64 = 200;

const STATE_DISCONNECTED: &str = "Disconnected";
const STATE_WAITING: &str = "Waiting for SRT feedback";
const STATE_HOLD: &str = "Hold";
const STATE_SETTLING: &str = "Settling";
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
        });
        while self
            .history
            .front()
            .is_some_and(|p| p.t_ms + HISTORY_MS < now_ms)
        {
            self.history.pop_front();
        }
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
    healthy: bool,
}

#[derive(Clone, Debug)]
pub struct AbrController {
    config: AbrConfig,
    internal_bitrate_bps: u64,
    last_srt_sample_ms: Option<u64>,
    estimator: Estimator,
    ceiling_bps: Option<u64>,
    /// Ceiling before the last collapse and when it was recorded.
    recovery: Option<(u64, u64)>,
    restored_at_ms: Option<u64>,
    /// Ceiling when the current congestion episode began.
    episode_ceiling_bps: Option<u64>,
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
            recovery: None,
            restored_at_ms: None,
            episode_ceiling_bps: None,
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
        self.last_control_state = STATE_WAITING;
        self.signals = Signals::default();
    }

    /// Forget everything learned about the path (new output session).
    pub fn forget_capacity(&mut self) {
        self.ceiling_bps = None;
        self.probe_dwell_ms = PROBE_DWELL_MS;
        self.recovery = None;
        self.restored_at_ms = None;
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
        self.recovery = None;
        self.restored_at_ms = None;
        self.probe_permille = PROBE_INITIAL_PERMILLE;
        self.probe_dwell_since_ms = None;
        self.probe_dwell_ms = PROBE_DWELL_MS;
        self.estimator = estimator.carry_baselines();
        self.last_srt_sample_ms = None;
        self.next_increase_ms = 0;
        self.settle_until_ms = 0;
        self.increase_hold_until_ms = now_ms.saturating_add(STARTUP_HOLD_MS);
        self.elevated_since_ms = None;
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
        let settling = now < self.settle_until_ms;
        let escalate = |target: u64| target < before * ESCALATE_BELOW_PERMILLE / 1_000;

        let mut emergency = false;
        let mut stressed = false;
        if signals.severe && before > self.config.min_bps {
            stressed = true;
            let by_delivery = delivery_target(SEVERE_DELIVERY_FACTOR, SEVERE_DELIVERY_FLOOR);
            let target = by_delivery.map_or(before / 2, |t| t.min(before * 9 / 10));
            if !settling || by_delivery.is_some_and(escalate) {
                self.decrease(now, target, video_goodput);
                self.last_control_state = STATE_SEVERE_CONGESTION;
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
                self.decrease(now, target, video_goodput);
                self.last_control_state = STATE_HEAVY_CONGESTION;
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
                self.decrease(now, target, video_goodput.map(|g| g.min(before)));
                self.last_control_state = STATE_LIGHT_CONGESTION;
            } else {
                self.last_control_state = STATE_SETTLING;
            }
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
        let rising = trend > 4.0_f64.max(dev * 1.5);
        let building = ratio < 0.92;
        let elevated = rtt > light_above_ms || queue > queue_light_ms;

        let now = sample.now_ms;
        let elevated_for = if elevated {
            let since = *self.elevated_since_ms.get_or_insert(now);
            now - since
        } else {
            self.elevated_since_ms = None;
            0
        };

        let severe = rtt >= severe_rtt_ms || queue >= queue_severe_ms;
        let heavy = rtt > heavy_above_ms
            || (rtt > latency / 5.0 && rtt > light_above_ms)
            || queue > queue_heavy_ms
            || (ratio < 0.75 && rtt > light_above_ms);
        let light = elevated && (rising || building || elevated_for >= ELEVATED_PERSIST_MS);
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
            healthy,
        }
    }

    fn decrease(&mut self, now: u64, target: u64, video_goodput: Option<u64>) {
        let before = self.internal_bitrate_bps;
        let target = target.clamp(self.config.min_bps, before.max(self.config.min_bps));
        // A decrease after the previous one's hold has expired starts a new
        // congestion episode.
        if now >= self.increase_hold_until_ms {
            self.episode_ceiling_bps = self.ceiling_bps;
        }
        if target < before {
            // The delivery rate measured while congested is the capacity;
            // the current target may already have been cut below it.
            let congestion_point = video_goodput.unwrap_or(before).max(self.config.min_bps);
            if let Some(previous) = self.episode_ceiling_bps {
                let failed_restore = self
                    .restored_at_ms
                    .is_some_and(|at| now < at + RESTORE_GRACE_MS);
                if congestion_point < previous * COLLAPSE_PERMILLE / 1_000 && !failed_restore {
                    let remembered = self.recovery.map_or(previous, |(r, _)| r.max(previous));
                    self.recovery = Some((remembered, now));
                } else if failed_restore {
                    self.recovery = None;
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
                if let Some((remembered, at)) = self.recovery {
                    if now >= at + RECOVERY_MEMORY_MS || remembered <= ceiling {
                        self.recovery = None;
                    } else if now >= dwell_since + RECOVERY_DWELL_MS {
                        // The collapse has passed: retry the previous
                        // capacity with the fast ramp instead of creeping up.
                        self.recovery = None;
                        self.restored_at_ms = Some(now);
                        self.ceiling_bps = Some(remembered);
                        self.probe_dwell_since_ms = None;
                        self.last_control_state = STATE_INCREASING;
                        return;
                    }
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
        let later = abr.decide(&sample(now + 2_000, held.applied_bps, 220));

        assert_eq!(first.control_state, STATE_HEAVY_CONGESTION);
        assert_eq!(held.applied_bps, first.applied_bps);
        assert_eq!(held.control_state, STATE_SETTLING);
        assert!(later.applied_bps < held.applied_bps);
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
