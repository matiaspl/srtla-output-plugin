use serde::{Deserialize, Serialize};

// Behavioral reference: BELABOX belacoder's GPL-3.0 congestion controller.
// This Rust implementation integrates the same signals and control law with
// the engine's stable ABI and OBS encoder lifecycle.
// https://github.com/BELABOX/belacoder/blob/master/belacoder.c

const DEFAULT_MIN_BPS: u64 = 500_000;
const DEFAULT_START_BPS: u64 = 1_500_000;
const BITRATE_INCREASE_MIN_BPS: u64 = 30_000;
const BITRATE_INCREASE_INTERVAL_MS: u64 = 500;
const BITRATE_INCREASE_SCALE: u64 = 30;
const BITRATE_DECREASE_MIN_BPS: u64 = 100_000;
const BITRATE_DECREASE_INTERVAL_MS: u64 = 200;
const BITRATE_DECREASE_FAST_INTERVAL_MS: u64 = 250;
const BITRATE_DECREASE_SCALE: u64 = 10;
const ROUNDING_BPS: u64 = 100_000;
const SRT_PAYLOAD_BYTES: f64 = 1_316.0;

const STATE_DISCONNECTED: &str = "Disconnected";
const STATE_WAITING: &str = "Waiting for SRT feedback";
const STATE_HOLD: &str = "Hold";
const STATE_INCREASING: &str = "Increasing";
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
    // Kept in the published telemetry shape, but no longer ABR inputs.
    // SRTLA link CC schedules traffic; it does not prove end-to-end capacity.
    pub link_capacity_bps: u64,
    pub srt_capacity_bps: u64,
    pub estimated_capacity_bps: u64,
    pub media_budget_bps: u64,
    pub recommended_bps: u64,
    pub applied_bps: u64,
    pub changed: bool,
    pub emergency: bool,
    pub srt_limited: bool,
    pub transport_stressed: bool,
    pub control_state: String,
    pub queue_light_packets: u32,
    pub queue_heavy_packets: u32,
    pub queue_severe_packets: u32,
    pub rtt_increase_below_ms: u32,
    pub rtt_decrease_above_ms: u32,
}

#[derive(Clone, Debug)]
pub struct AbrController {
    config: AbrConfig,
    internal_bitrate_bps: u64,
    last_srt_sample_ms: Option<u64>,
    send_buffer_average: f64,
    send_buffer_jitter: f64,
    previous_send_buffer: u32,
    rtt_average: f64,
    rtt_average_delta: f64,
    previous_rtt_ms: u32,
    rtt_minimum: f64,
    rtt_jitter: f64,
    throughput: f64,
    next_increase_ms: u64,
    next_decrease_ms: u64,
    last_control_state: &'static str,
    queue_light_packets: u32,
    queue_heavy_packets: u32,
    queue_severe_packets: u32,
    rtt_increase_below_ms: u32,
    rtt_decrease_above_ms: u32,
}

impl AbrController {
    pub fn new(config: AbrConfig) -> Self {
        let config = config.normalized();
        let internal_bitrate_bps = config.start_bps;
        Self {
            config,
            internal_bitrate_bps,
            last_srt_sample_ms: None,
            send_buffer_average: 0.0,
            send_buffer_jitter: 0.0,
            previous_send_buffer: 0,
            rtt_average: 0.0,
            rtt_average_delta: 0.0,
            previous_rtt_ms: 300,
            rtt_minimum: 200.0,
            rtt_jitter: 0.0,
            throughput: 0.0,
            next_increase_ms: 0,
            next_decrease_ms: 0,
            last_control_state: STATE_WAITING,
            queue_light_packets: 0,
            queue_heavy_packets: 0,
            queue_severe_packets: 0,
            rtt_increase_below_ms: 0,
            rtt_decrease_above_ms: 0,
        }
    }

    pub fn config(&self) -> &AbrConfig {
        &self.config
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

    pub fn reset_for_bitrate(&mut self, current_bps: u64) {
        self.internal_bitrate_bps = current_bps.clamp(self.config.min_bps, self.config.max_bps);
        self.last_srt_sample_ms = None;
        self.send_buffer_average = 0.0;
        self.send_buffer_jitter = 0.0;
        self.previous_send_buffer = 0;
        self.rtt_average = 0.0;
        self.rtt_average_delta = 0.0;
        self.previous_rtt_ms = 300;
        self.rtt_minimum = 200.0;
        self.rtt_jitter = 0.0;
        self.throughput = 0.0;
        self.next_increase_ms = 0;
        self.next_decrease_ms = 0;
        self.last_control_state = STATE_WAITING;
        self.queue_light_packets = 0;
        self.queue_heavy_packets = 0;
        self.queue_severe_packets = 0;
        self.rtt_increase_below_ms = 0;
        self.rtt_decrease_above_ms = 0;
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

        self.update_measurements(&sample.srt);
        self.update_thresholds(&sample.srt);

        let rtt_ms = sample.srt.rtt_ms;
        let send_buffer_packets = sample.srt.send_buffer_packets;
        let severe =
            rtt_ms >= sample.srt.latency_ms / 3 || send_buffer_packets > self.queue_severe_packets;
        let heavy =
            rtt_ms > sample.srt.latency_ms / 5 || send_buffer_packets > self.queue_heavy_packets;
        let light =
            rtt_ms > self.rtt_decrease_above_ms || send_buffer_packets > self.queue_light_packets;
        let may_increase = rtt_ms < self.rtt_increase_below_ms && self.rtt_average_delta < 0.01;

        let mut emergency = false;
        let mut stressed = false;
        if self.internal_bitrate_bps > self.config.min_bps && severe {
            self.internal_bitrate_bps = self.config.min_bps;
            self.next_decrease_ms = sample.now_ms.saturating_add(BITRATE_DECREASE_INTERVAL_MS);
            self.last_control_state = STATE_SEVERE_CONGESTION;
            emergency = true;
            stressed = true;
        } else if sample.now_ms > self.next_decrease_ms && heavy {
            let decrease = BITRATE_DECREASE_MIN_BPS
                .saturating_add(self.internal_bitrate_bps / BITRATE_DECREASE_SCALE);
            self.internal_bitrate_bps = self.internal_bitrate_bps.saturating_sub(decrease);
            self.next_decrease_ms = sample
                .now_ms
                .saturating_add(BITRATE_DECREASE_FAST_INTERVAL_MS);
            self.last_control_state = STATE_HEAVY_CONGESTION;
            stressed = true;
        } else if sample.now_ms > self.next_decrease_ms && light {
            self.internal_bitrate_bps = self
                .internal_bitrate_bps
                .saturating_sub(BITRATE_DECREASE_MIN_BPS);
            self.next_decrease_ms = sample.now_ms.saturating_add(BITRATE_DECREASE_INTERVAL_MS);
            self.last_control_state = STATE_LIGHT_CONGESTION;
            stressed = true;
        } else if sample.now_ms > self.next_increase_ms && may_increase {
            let increase = BITRATE_INCREASE_MIN_BPS
                .saturating_add(self.internal_bitrate_bps / BITRATE_INCREASE_SCALE);
            self.internal_bitrate_bps = self.internal_bitrate_bps.saturating_add(increase);
            self.next_increase_ms = sample.now_ms.saturating_add(BITRATE_INCREASE_INTERVAL_MS);
            self.last_control_state = STATE_INCREASING;
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

    fn update_measurements(&mut self, srt: &SrtCapacity) {
        let send_buffer = f64::from(srt.send_buffer_packets);
        self.send_buffer_average = self.send_buffer_average * 0.99 + send_buffer * 0.01;
        self.send_buffer_jitter *= 0.99;
        let buffer_delta =
            f64::from(srt.send_buffer_packets) - f64::from(self.previous_send_buffer);
        if buffer_delta > self.send_buffer_jitter {
            self.send_buffer_jitter = buffer_delta;
        }
        self.previous_send_buffer = srt.send_buffer_packets;

        let rtt = f64::from(srt.rtt_ms);
        if self.rtt_average == 0.0 {
            self.rtt_average = rtt;
        } else {
            self.rtt_average = self.rtt_average * 0.99 + rtt * 0.01;
        }
        let rtt_delta = rtt - f64::from(self.previous_rtt_ms);
        self.rtt_average_delta = self.rtt_average_delta * 0.8 + rtt_delta * 0.2;
        self.previous_rtt_ms = srt.rtt_ms;

        self.rtt_minimum *= 1.001;
        if srt.rtt_ms != 100 && rtt < self.rtt_minimum && self.rtt_average_delta < 1.0 {
            self.rtt_minimum = rtt;
        }
        self.rtt_jitter *= 0.99;
        if rtt_delta > self.rtt_jitter {
            self.rtt_jitter = rtt_delta;
        }

        // Preserve belacoder's near-kilobits-per-second conversion before
        // translating half the latency window to packets.
        let throughput_sample = srt.send_rate_bps as f64 / 1_024.0;
        self.throughput = self.throughput * 0.97 + throughput_sample * 0.03;
    }

    fn update_thresholds(&mut self, srt: &SrtCapacity) {
        self.queue_severe_packets =
            to_u32((self.send_buffer_average + self.send_buffer_jitter) * 4.0);

        let queue_heavy = 50.0_f64.max(
            self.send_buffer_average
                + (self.send_buffer_jitter * 3.0).max(self.send_buffer_average),
        );
        let half_latency_packets =
            (self.throughput / 8.0) * f64::from(srt.latency_ms / 2) / SRT_PAYLOAD_BYTES;
        self.queue_heavy_packets = to_u32(queue_heavy.min(half_latency_packets));
        self.queue_light_packets =
            to_u32(50.0_f64.max(self.send_buffer_average + self.send_buffer_jitter * 2.5));
        self.rtt_decrease_above_ms =
            to_u32(self.rtt_average + (self.rtt_jitter * 4.0).max(self.rtt_average * 0.15));
        self.rtt_increase_below_ms = to_u32(self.rtt_minimum + 1.0_f64.max(self.rtt_jitter * 2.0));
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
        AbrDecision {
            link_capacity_bps,
            srt_capacity_bps: 0,
            estimated_capacity_bps: 0,
            media_budget_bps: self.internal_bitrate_bps.saturating_add(sample.audio_bps),
            recommended_bps: applied_bps,
            applied_bps,
            changed: applied_bps != current_bps,
            emergency,
            srt_limited: false,
            transport_stressed: stressed,
            control_state: self.last_control_state.to_string(),
            queue_light_packets: self.queue_light_packets,
            queue_heavy_packets: self.queue_heavy_packets,
            queue_severe_packets: self.queue_severe_packets,
            rtt_increase_below_ms: self.rtt_increase_below_ms,
            rtt_decrease_above_ms: self.rtt_decrease_above_ms,
        }
    }
}

fn to_u32(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else if value >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        value as u32
    }
}

fn rounded_bps(value: u64) -> u64 {
    value / ROUNDING_BPS * ROUNDING_BPS
}

#[cfg(test)]
mod tests {
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
                send_buffer_ms: 0,
                send_buffer_packets: 0,
                rtt_ms,
                latency_ms: 2_000,
                retransmit_permille: 0,
                dropped_bytes: 0,
            },
        }
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
        assert_eq!(decision.estimated_capacity_bps, 0);
    }

    #[test]
    fn severe_rtt_congestion_drops_immediately_to_the_floor() {
        let mut abr = controller(6_000_000);
        let decision = abr.decide(&sample(20, 6_000_000, 700));

        assert_eq!(decision.applied_bps, 500_000);
        assert!(decision.emergency);
        assert_eq!(decision.control_state, STATE_SEVERE_CONGESTION);
    }

    #[test]
    fn heavy_rtt_congestion_uses_belabox_decrease_step() {
        let mut abr = controller(6_000_000);
        let decision = abr.decide(&sample(20, 6_000_000, 450));

        assert_eq!(decision.applied_bps, 5_300_000);
        assert!(decision.transport_stressed);
        assert_eq!(decision.control_state, STATE_HEAVY_CONGESTION);
    }

    #[test]
    fn sender_queue_growth_drives_congestion_without_a_bandwidth_estimate() {
        let mut abr = controller(6_000_000);
        let first = abr.decide(&sample(20, 6_000_000, 50));
        let mut queued = sample(40, first.applied_bps, 50);
        queued.srt.bandwidth_bps = 0;
        queued.srt.send_buffer_packets = 100;

        let decision = abr.decide(&queued);

        assert_eq!(decision.control_state, STATE_HEAVY_CONGESTION);
        assert!(decision.applied_bps < first.applied_bps);
        assert_eq!(decision.srt_capacity_bps, 0);
    }

    #[test]
    fn heavy_decreases_are_rate_limited() {
        let mut abr = controller(6_000_000);
        let first = abr.decide(&sample(20, 6_000_000, 450));
        let held = abr.decide(&sample(40, first.applied_bps, 450));
        let second = abr.decide(&sample(280, held.applied_bps, 450));

        assert_eq!(first.applied_bps, 5_300_000);
        assert_eq!(held.applied_bps, first.applied_bps);
        assert!(second.applied_bps < held.applied_bps);
    }

    #[test]
    fn healthy_feedback_accumulates_sub_rounding_increases() {
        let mut abr = controller(1_500_000);
        let mut current = 1_500_000;

        for now_ms in (20..=1_100).step_by(20) {
            let decision = abr.decide(&sample(now_ms, current, 50));
            current = decision.applied_bps;
        }

        assert!(current > 1_500_000);
        assert_eq!(current % ROUNDING_BPS, 0);
    }

    #[test]
    fn repeated_srt_sample_is_not_applied_twice() {
        let mut abr = controller(6_000_000);
        let first_sample = sample(20, 6_000_000, 450);
        let first = abr.decide(&first_sample);
        assert_eq!(first.applied_bps, 5_300_000);

        let mut repeated = first_sample;
        repeated.now_ms = 300;
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
        assert_eq!(abr.set_current_bps(4_000_000), 4_000_000);

        let decision = abr.decide(&sample(20, 4_000_000, 450));

        assert_eq!(decision.applied_bps, 3_500_000);
    }
}
