use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

// Behavioral reference: BELABOX belacoder's GPL-3.0 congestion controller.
// This Rust implementation integrates the same signals and control law with
// the engine's stable ABI and OBS encoder lifecycle.
// https://github.com/BELABOX/belacoder/blob/master/belacoder.c
//
// The control law now departs from belacoder in four ways:
// 1. Heavy and severe decreases are anchored to a windowed delivery-rate
//    estimate (conservation of flow over the SRT send buffer) instead of
//    blind steps or an unconditional drop to the floor. The estimate is only
//    trusted on decreases: while uncongested it merely equals what was offered.
// 2. The interval between decreases scales with RTT, so a cut can show up in
//    feedback before the next one is applied.
// 3. The bitrate in use before a congestion episode is remembered, and
//    increases move quickly back toward it once the link recovers.
// 4. Increases are gated on the encoder actually filling the pipe, so a static
//    scene does not walk the target upward without evidence.

const DEFAULT_MIN_BPS: u64 = 500_000;
const DEFAULT_START_BPS: u64 = 1_500_000;
const BITRATE_INCREASE_MIN_BPS: u64 = 30_000;
const BITRATE_INCREASE_INTERVAL_MS: u64 = 500;
const BITRATE_INCREASE_SCALE: u64 = 30;
const BITRATE_DECREASE_MIN_BPS: u64 = 100_000;
const BITRATE_DECREASE_INTERVAL_MS: u64 = 200;
const BITRATE_DECREASE_FAST_INTERVAL_MS: u64 = 250;
const BITRATE_DECREASE_SCALE: u64 = 10;
const ENCODER_REACTION_MS: u64 = 100;
const DECREASE_INTERVAL_MAX_MS: u64 = 1_000;
const RATE_WINDOW_MS: u64 = 1_000;
const RATE_WINDOW_MIN_MS: u64 = 500;
const HEAVY_CAPACITY_FRACTION: f64 = 0.85;
const SEVERE_CAPACITY_FRACTION: f64 = 0.50;
const FAST_RECOVERY_GAP_DIVISOR: u64 = 4;
const FAST_RECOVERY_STEP_DIVISOR: u64 = 4;
const OFFERED_FILL_FRACTION_PERCENT: u64 = 80;
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
    #[serde(default)]
    pub sent_unique_bytes: u64,
    #[serde(default)]
    pub packets_in_flight: u32,
}

#[derive(Clone, Debug)]
struct RateSample {
    sampled_at_ms: u64,
    sent_unique_bytes: u64,
    dropped_bytes: u64,
    unacked_packets: u32,
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
    rate_window: VecDeque<RateSample>,
    consecutive_severe: u32,
    last_good_bps: u64,
    in_congestion_episode: bool,
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
            rate_window: VecDeque::new(),
            consecutive_severe: 0,
            last_good_bps: 0,
            in_congestion_episode: false,
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
        self.last_good_bps = self.last_good_bps.min(max_bps);
        max_bps
    }

    /// Synchronize with a bitrate selected outside ABR, such as the value in
    /// use when automatic mode is enabled.
    pub fn set_current_bps(&mut self, current_bps: u64) -> u64 {
        let current_bps = current_bps.clamp(self.config.min_bps, self.config.max_bps);
        self.internal_bitrate_bps = current_bps;
        self.last_good_bps = 0;
        self.in_congestion_episode = false;
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
        self.rate_window.clear();
        self.consecutive_severe = 0;
        self.last_good_bps = 0;
        self.in_congestion_episode = false;
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
            self.begin_congestion_episode();
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

        let delivered = self.delivered_bps();
        let audio_bps = sample.audio_bps;
        let can_decrease = sample.now_ms > self.next_decrease_ms;
        let interval = |base_ms: u64| {
            (u64::from(rtt_ms) + ENCODER_REACTION_MS).clamp(base_ms, DECREASE_INTERVAL_MAX_MS)
        };
        if !severe {
            self.consecutive_severe = 0;
        }

        let mut emergency = false;
        let mut stressed = false;
        if self.internal_bitrate_bps > self.config.min_bps && severe && can_decrease {
            self.begin_congestion_episode();
            self.internal_bitrate_bps = match delivered {
                // A second severe decision in a row means the capacity cut was
                // not enough: stop trusting the estimate and go to the floor.
                Some(rate) if self.consecutive_severe == 0 => self
                    .internal_bitrate_bps
                    .min(video_cap(rate, SEVERE_CAPACITY_FRACTION, audio_bps)),
                _ => self.config.min_bps,
            };
            self.consecutive_severe = self.consecutive_severe.saturating_add(1);
            self.next_decrease_ms = sample
                .now_ms
                .saturating_add(interval(BITRATE_DECREASE_INTERVAL_MS));
            self.last_control_state = STATE_SEVERE_CONGESTION;
            emergency = true;
            stressed = true;
        } else if can_decrease && heavy {
            self.begin_congestion_episode();
            let decrease = BITRATE_DECREASE_MIN_BPS
                .saturating_add(self.internal_bitrate_bps / BITRATE_DECREASE_SCALE);
            let stepped = self.internal_bitrate_bps.saturating_sub(decrease);
            self.internal_bitrate_bps = match delivered {
                Some(rate) => stepped.min(video_cap(rate, HEAVY_CAPACITY_FRACTION, audio_bps)),
                None => stepped,
            };
            self.next_decrease_ms = sample
                .now_ms
                .saturating_add(interval(BITRATE_DECREASE_FAST_INTERVAL_MS));
            self.last_control_state = STATE_HEAVY_CONGESTION;
            stressed = true;
        } else if can_decrease && light {
            if !self.in_congestion_episode {
                // Congestion returned after recovery had begun: the remembered
                // rate is not currently achievable, so stop chasing it.
                self.last_good_bps = 0;
            }
            self.internal_bitrate_bps = self
                .internal_bitrate_bps
                .saturating_sub(BITRATE_DECREASE_MIN_BPS);
            self.next_decrease_ms = sample
                .now_ms
                .saturating_add(interval(BITRATE_DECREASE_INTERVAL_MS));
            self.last_control_state = STATE_LIGHT_CONGESTION;
            stressed = true;
        } else if sample.now_ms > self.next_increase_ms
            && may_increase
            && self.increase_allowed(audio_bps)
        {
            let normal = BITRATE_INCREASE_MIN_BPS
                .saturating_add(self.internal_bitrate_bps / BITRATE_INCREASE_SCALE);
            let target = self.recovery_target();
            let increase = if self.internal_bitrate_bps < target {
                let gap = (target - self.internal_bitrate_bps) / FAST_RECOVERY_GAP_DIVISOR;
                normal.max(gap.min(self.internal_bitrate_bps / FAST_RECOVERY_STEP_DIVISOR))
            } else {
                normal
            };
            self.internal_bitrate_bps = self.internal_bitrate_bps.saturating_add(increase);
            self.in_congestion_episode = false;
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

    /// Remember the bitrate that worked before the first cut of an episode.
    fn begin_congestion_episode(&mut self) {
        if !self.in_congestion_episode {
            self.last_good_bps = self.internal_bitrate_bps;
            self.in_congestion_episode = true;
        }
    }

    fn recovery_target(&self) -> u64 {
        self.last_good_bps / 10 * 9
    }

    /// Raise the target only when returning to a rate that worked before, or
    /// when the encoder is actually filling the pipe.
    fn increase_allowed(&self, audio_bps: u64) -> bool {
        if self.last_good_bps > 0 && self.internal_bitrate_bps < self.recovery_target() {
            return true;
        }
        self.offered_bps().is_some_and(|offered| {
            u128::from(offered) * 100
                >= u128::from(OFFERED_FILL_FRACTION_PERCENT)
                    * u128::from(self.internal_bitrate_bps.saturating_add(audio_bps))
        })
    }

    /// Span of the rate window and the sums over samples after its oldest
    /// entry, or `None` until the window is long enough to trust.
    fn window_totals(&self) -> Option<(u64, u64, u64, i64)> {
        let oldest = self.rate_window.front()?;
        let newest = self.rate_window.back()?;
        let span_ms = newest.sampled_at_ms.saturating_sub(oldest.sampled_at_ms);
        if span_ms < RATE_WINDOW_MIN_MS {
            return None;
        }
        let (sent, dropped) =
            self.rate_window
                .iter()
                .skip(1)
                .fold((0_u64, 0_u64), |(sent, dropped), entry| {
                    (
                        sent.saturating_add(entry.sent_unique_bytes),
                        dropped.saturating_add(entry.dropped_bytes),
                    )
                });
        let unacked_delta = i64::from(newest.unacked_packets) - i64::from(oldest.unacked_packets);
        Some((span_ms, sent, dropped, unacked_delta))
    }

    /// Bits per second handed to the SRT sender over the window.
    fn offered_bps(&self) -> Option<u64> {
        let (span_ms, sent, _, _) = self.window_totals()?;
        Some(sent.saturating_mul(8_000) / span_ms)
    }

    /// Bits per second acknowledged by the receiver over the window:
    /// everything sent, minus the growth of unacknowledged packets, minus what
    /// the sender dropped. Equals the path capacity only while congested; when
    /// uncongested it just equals what was offered, so it is used on decreases
    /// only.
    fn delivered_bps(&self) -> Option<u64> {
        let (span_ms, sent, dropped, unacked_delta) = self.window_totals()?;
        let delivered_bits = i128::from(sent) * 8
            - i128::from(unacked_delta) * SRT_PAYLOAD_BYTES as i128 * 8
            - i128::from(dropped) * 8;
        let delivered_bits = delivered_bits.max(0);
        Some((delivered_bits * 1_000 / i128::from(span_ms)) as u64)
    }

    fn update_measurements(&mut self, srt: &SrtCapacity) {
        self.rate_window.push_back(RateSample {
            sampled_at_ms: srt.sampled_at_ms,
            sent_unique_bytes: srt.sent_unique_bytes,
            dropped_bytes: srt.dropped_bytes,
            unacked_packets: srt.send_buffer_packets,
        });
        while self.rate_window.len() > 2
            && srt.sampled_at_ms.saturating_sub(
                self.rate_window
                    .front()
                    .map_or(srt.sampled_at_ms, |entry| entry.sampled_at_ms),
            ) > RATE_WINDOW_MS
        {
            self.rate_window.pop_front();
        }

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
            estimated_capacity_bps: self.delivered_bps().unwrap_or(0),
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

fn video_cap(delivered_bps: u64, fraction: f64, audio_bps: u64) -> u64 {
    ((delivered_bps as f64 * fraction) as u64).saturating_sub(audio_bps)
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
                sent_unique_bytes: (current_bps + 128_000) * 20 / 8_000,
                packets_in_flight: 0,
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
        // At RTT 450 the decrease interval is 450 + 100 = 550 ms.
        let still_held = abr.decide(&sample(300, held.applied_bps, 450));
        let second = abr.decide(&sample(600, still_held.applied_bps, 450));

        assert_eq!(first.applied_bps, 5_300_000);
        assert_eq!(held.applied_bps, first.applied_bps);
        assert_eq!(still_held.applied_bps, first.applied_bps);
        assert_eq!(still_held.control_state, STATE_HOLD);
        assert!(second.applied_bps < still_held.applied_bps);
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

    // ---- Fluid-link simulator -------------------------------------------

    const SIM_DT_MS: u64 = 20;
    const SIM_AUDIO_BPS: f64 = 128_000.0;

    struct Sim {
        abr: AbrController,
        capacity_bps: f64,
        base_rtt_ms: f64,
        fill: f64,
        queue_bits: f64,
        now_ms: u64,
        applied_bps: u64,
    }

    impl Sim {
        fn new(
            start_bps: u64,
            max_bps: u64,
            capacity_bps: f64,
            base_rtt_ms: f64,
            fill: f64,
        ) -> Self {
            Self {
                abr: AbrController::new(AbrConfig {
                    min_bps: 500_000,
                    start_bps,
                    max_bps,
                }),
                capacity_bps,
                base_rtt_ms,
                fill,
                queue_bits: 0.0,
                now_ms: 0,
                applied_bps: start_bps,
            }
        }

        fn step(&mut self) -> AbrDecision {
            let dt = SIM_DT_MS as f64 / 1_000.0;
            let lambda = self.fill * self.applied_bps as f64 + SIM_AUDIO_BPS;
            let capacity = self.capacity_bps;
            self.queue_bits = (self.queue_bits + (lambda - capacity) * dt).max(0.0);
            let rtt_ms = self.base_rtt_ms + self.queue_bits / capacity * 1_000.0;
            let unacked = (lambda * self.base_rtt_ms / 1_000.0 + self.queue_bits) / (1_316.0 * 8.0);
            self.now_ms += SIM_DT_MS;
            let input = AbrSample {
                links: Vec::new(),
                audio_bps: SIM_AUDIO_BPS as u64,
                current_video_bps: self.applied_bps,
                now_ms: self.now_ms,
                capacity_ready: true,
                all_links_down: false,
                srt: SrtCapacity {
                    ready: true,
                    sampled_at_ms: self.now_ms,
                    bandwidth_bps: 0,
                    send_rate_bps: lambda as u64,
                    send_buffer_ms: 0,
                    send_buffer_packets: unacked as u32,
                    rtt_ms: (rtt_ms.ceil() as u32).max(1),
                    latency_ms: 2_000,
                    retransmit_permille: 0,
                    dropped_bytes: 0,
                    sent_unique_bytes: (lambda * dt / 8.0) as u64,
                    packets_in_flight: 0,
                },
            };
            let decision = self.abr.decide(&input);
            self.applied_bps = decision.applied_bps;
            decision
        }

        /// Run for `seconds`, returning (min, max, last) of the applied
        /// bitrate observed once `observe_after_s` seconds have elapsed.
        fn run(&mut self, seconds: u64, observe_after_s: u64) -> (u64, u64, u64) {
            let start = self.now_ms;
            let mut min = u64::MAX;
            let mut max = 0;
            for _ in 0..(seconds * 1_000 / SIM_DT_MS) {
                self.step();
                if self.now_ms - start >= observe_after_s * 1_000 {
                    min = min.min(self.applied_bps);
                    max = max.max(self.applied_bps);
                }
            }
            (min, max, self.applied_bps)
        }
    }

    #[test]
    fn sim_capacity_step_settles_below_capacity_without_hitting_the_floor() {
        let mut sim = Sim::new(6_000_000, 6_000_000, 8_000_000.0, 50.0, 1.0);
        sim.run(30, 0);
        sim.capacity_bps = 3_000_000.0;
        let (min_early, _, _) = sim.run(10, 0);
        let (min, max, _) = sim.run(20, 0);

        assert!(min_early > 500_000, "hit the floor: {min_early}");
        assert!(min >= 1_800_000, "below 0.6C: {min}");
        assert!(max <= 3_000_000, "above 1.0C: {max}");
    }

    #[test]
    fn sim_transient_outage_recovers_quickly() {
        let mut sim = Sim::new(6_000_000, 6_000_000, 8_000_000.0, 50.0, 1.0);
        sim.run(30, 0);
        sim.capacity_bps = 300_000.0;
        sim.run(3, 0);
        sim.capacity_bps = 8_000_000.0;
        let (_, _, last) = sim.run(10, 10);

        assert!(last >= 5_000_000, "recovered only to {last}");
    }

    #[test]
    fn sim_underfilled_encoder_does_not_raise_the_target() {
        let mut sim = Sim::new(2_000_000, 20_000_000, 50_000_000.0, 50.0, 0.3);
        let (_, max, _) = sim.run(60, 0);

        assert!(max <= 2_000_000, "target rose to {max}");
    }

    #[test]
    fn sim_high_rtt_capacity_step_stays_above_half_capacity() {
        let mut sim = Sim::new(6_000_000, 6_000_000, 8_000_000.0, 300.0, 1.0);
        sim.run(30, 0);
        sim.capacity_bps = 3_000_000.0;
        let (min, _, _) = sim.run(30, 0);

        assert!(min >= 1_500_000, "fell to {min}");
    }

    fn rate_entry(abr: &mut AbrController, at_ms: u64, sent: u64, unacked: u32) {
        let mut srt = sample(at_ms, 4_000_000, 50).srt;
        srt.sent_unique_bytes = sent;
        srt.send_buffer_packets = unacked;
        abr.update_measurements(&srt);
    }

    #[test]
    fn delivered_rate_window_applies_conservation_of_flow() {
        let mut abr = controller(4_000_000);
        // 4 Mbps offered in 20 ms slices; the unacked count grows by one
        // packet per slice.
        for i in 0..=25_u64 {
            rate_entry(&mut abr, 20 + i * 20, 10_000, i as u32);
        }
        // 25 slices after the oldest span only 500 ms: just valid.
        assert_eq!(abr.offered_bps(), Some(4_000_000));
        let expected = (25 * 10_000 * 8 - 25 * 1_316 * 8) * 2;
        assert_eq!(abr.delivered_bps(), Some(expected));

        let mut short = controller(4_000_000);
        for i in 0..=24_u64 {
            rate_entry(&mut short, 20 + i * 20, 10_000, i as u32);
        }
        assert_eq!(short.delivered_bps(), None);
        assert_eq!(short.offered_bps(), None);
    }

    #[test]
    fn second_consecutive_severe_goes_to_the_floor() {
        let mut abr = controller(6_000_000);
        let mut current = 6_000_000;
        for now_ms in (20..=1_200).step_by(20) {
            let decision = abr.decide(&sample(now_ms, current, 50));
            current = decision.applied_bps;
        }
        // Warm the delivery window, then report severe RTT twice.
        let first = abr.decide(&sample(1_220, current, 700));
        assert_eq!(first.control_state, STATE_SEVERE_CONGESTION);
        assert!(first.applied_bps > 500_000, "first severe should not floor");

        let mut next = 1_220;
        let second = loop {
            next += 20;
            let decision = abr.decide(&sample(next, first.applied_bps, 700));
            if decision.control_state == STATE_SEVERE_CONGESTION {
                break decision;
            }
            assert!(next < 3_000, "second severe never fired");
        };
        assert_eq!(second.applied_bps, 500_000);
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
