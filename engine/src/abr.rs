use serde::{Deserialize, Serialize};

const DEFAULT_SAFETY_MARGIN: f64 = 0.80;
const DEFAULT_MIN_BPS: u64 = 500_000;
const DEFAULT_START_BPS: u64 = 1_500_000;
const DEFAULT_UP_HEADROOM: f64 = 1.15;
const UP_STABILITY_TICKS: u32 = 10;
const UP_INTERVAL_TICKS: u32 = 5;
const DOWN_DEADBAND: f64 = 0.90;
const MAX_UP_STEP_BPS: u64 = 500_000;
const ROUNDING_BPS: u64 = 50_000;
const SRT_STATS_WARMUP_TICKS: u32 = 3;
const SRT_STALE_GROWTH_FREEZE_TICKS: u32 = 3;
const SRT_QUEUE_STRESS_MS: u32 = 250;
const SRT_QUEUE_REARM_MS: u32 = 50;
const SRT_RETRANS_STRESS_PERMILLE: u32 = 200;
const SRT_RETRANS_HEALTHY_PERMILLE: u32 = 100;
const SRT_QUEUE_BACKOFF_FACTOR: f64 = 0.80;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AbrConfig {
    #[serde(default = "default_safety_margin")]
    pub safety_margin: f64,
    #[serde(default = "default_min_bps")]
    pub min_bps: u64,
    #[serde(default = "default_start_bps")]
    pub start_bps: u64,
    pub max_bps: u64,
}

impl Default for AbrConfig {
    fn default() -> Self {
        Self {
            safety_margin: DEFAULT_SAFETY_MARGIN,
            min_bps: DEFAULT_MIN_BPS,
            start_bps: DEFAULT_START_BPS,
            max_bps: DEFAULT_START_BPS,
        }
    }
}

fn default_safety_margin() -> f64 {
    DEFAULT_SAFETY_MARGIN
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
            safety_margin: self.safety_margin.clamp(0.50, 0.95),
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
    pub send_buffer_ms: u32,
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
}

#[derive(Clone, Debug)]
pub struct AbrController {
    config: AbrConfig,
    stable_headroom_ticks: u32,
    ticks_since_up: u32,
    growth_freeze_ticks: u32,
    last_change_ms: Option<u64>,
    has_sample: bool,
    filtered_srt_bps: Option<u64>,
    last_srt_sample_ms: Option<u64>,
    srt_valid_ticks: u32,
    srt_backoff_armed: bool,
}

impl AbrController {
    pub fn new(config: AbrConfig) -> Self {
        Self {
            config: config.normalized(),
            stable_headroom_ticks: 0,
            ticks_since_up: UP_INTERVAL_TICKS,
            growth_freeze_ticks: 0,
            last_change_ms: None,
            has_sample: false,
            filtered_srt_bps: None,
            last_srt_sample_ms: None,
            srt_valid_ticks: 0,
            srt_backoff_armed: true,
        }
    }

    pub fn config(&self) -> &AbrConfig {
        &self.config
    }

    /// Change the live video ceiling without resetting learned capacity state.
    /// Raising the ceiling must earn fresh stable headroom before ABR grows,
    /// while lowering it is enforced by the next decision immediately.
    pub fn set_max_bps(&mut self, max_bps: u64) -> u64 {
        let max_bps = max_bps.max(self.config.min_bps).max(ROUNDING_BPS);
        if self.config.max_bps != max_bps {
            self.config.max_bps = max_bps;
            self.stable_headroom_ticks = 0;
        }
        max_bps
    }

    pub fn reset(&mut self) {
        self.stable_headroom_ticks = 0;
        self.ticks_since_up = UP_INTERVAL_TICKS;
        self.growth_freeze_ticks = 0;
        self.last_change_ms = None;
        self.has_sample = false;
        self.filtered_srt_bps = None;
        self.last_srt_sample_ms = None;
        self.srt_valid_ticks = 0;
        self.srt_backoff_armed = true;
    }

    /// Hold capacity increases for the warm-up period after a link is added
    /// or re-enabled.  Reductions remain active while the freeze is running.
    pub fn freeze_growth_ticks(&mut self, ticks: u32) {
        self.growth_freeze_ticks = self.growth_freeze_ticks.max(ticks);
        self.stable_headroom_ticks = 0;
    }

    fn update_srt_capacity(
        &mut self,
        srt: &SrtCapacity,
        delivered_bps: u64,
        current_media_bps: u64,
    ) -> Option<u64> {
        if !srt.ready {
            if self.filtered_srt_bps.is_some() {
                self.freeze_growth_ticks(SRT_STALE_GROWTH_FREEZE_TICKS);
            }
            self.filtered_srt_bps = None;
            self.last_srt_sample_ms = None;
            self.srt_valid_ticks = 0;
            self.srt_backoff_armed = true;
            return None;
        }
        if srt.bandwidth_bps == 0 {
            if self.filtered_srt_bps.is_some() {
                self.freeze_growth_ticks(SRT_STALE_GROWTH_FREEZE_TICKS);
            }
            self.filtered_srt_bps = None;
            self.last_srt_sample_ms = None;
            self.srt_valid_ticks = 0;
            return None;
        }

        if self
            .last_srt_sample_ms
            .is_some_and(|last| srt.sampled_at_ms < last)
        {
            return self
                .filtered_srt_bps
                .filter(|_| self.srt_valid_ticks >= SRT_STATS_WARMUP_TICKS);
        }
        if self.last_srt_sample_ms == Some(srt.sampled_at_ms) {
            return self
                .filtered_srt_bps
                .filter(|_| self.srt_valid_ticks >= SRT_STATS_WARMUP_TICKS);
        }
        self.last_srt_sample_ms = Some(srt.sampled_at_ms);

        // A healthy estimate is only useful as a capacity cap when it leaves
        // enough headroom for the load that is already known to work. SRT's
        // receiver-side packet-pair estimate is particularly noisy when SRTLA
        // routes the pair over two heterogeneous links. Merely requiring the
        // estimate to exceed delivered throughput is not sufficient: applying
        // the safety margin to (for example) a 1.05x estimate still requests a
        // lower encoder rate, then the estimate follows that lower load and the
        // controller ratchets down again.
        //
        // Reject a healthy, load-following estimate unless its safety-adjusted
        // value covers both the configured media rate and ACK-confirmed load.
        // Congested samples remain usable below this threshold; queue, drop,
        // and retransmission signals are independent evidence that a reduction
        // is warranted.
        let reduction_evidence = srt_transport_stressed(srt);
        let proven_load_bps = delivered_bps.max(current_media_bps);
        let required_headroom_bps =
            ((proven_load_bps as f64) / self.config.safety_margin).ceil() as u64;
        if !reduction_evidence && srt.bandwidth_bps < required_headroom_bps {
            self.filtered_srt_bps = None;
            self.srt_valid_ticks = 0;
            return None;
        }
        let sample_bps = srt.bandwidth_bps;

        self.filtered_srt_bps = Some(match self.filtered_srt_bps {
            None => sample_bps,
            Some(previous) if sample_bps < previous => {
                // Follow genuine contractions faster than expansions.
                blend_bps(previous, sample_bps, 1, 2)
            }
            Some(previous) => blend_bps(previous, sample_bps, 1, 5),
        });
        self.srt_valid_ticks = self.srt_valid_ticks.saturating_add(1);
        self.filtered_srt_bps
            .filter(|_| self.srt_valid_ticks >= SRT_STATS_WARMUP_TICKS)
    }

    pub fn decide(&mut self, sample: &AbrSample) -> AbrDecision {
        let link_capacity: u64 = sample
            .links
            .iter()
            .filter(|link| link.enabled && link.payload_eligible && link.capacity_ready)
            .map(|link| link.target_bps)
            .sum();
        let delivered_bps: u64 = sample
            .links
            .iter()
            .filter(|link| link.enabled && link.payload_eligible)
            .map(|link| link.delivered_bps)
            .sum();
        let current = sample
            .current_video_bps
            .clamp(self.config.min_bps, self.config.max_bps);
        let current_media_bps = current.saturating_add(sample.audio_bps);
        let transport_stressed = sample.srt.ready && srt_transport_stressed(&sample.srt);
        let transport_healthy = sample.srt.ready
            && sample.srt.send_buffer_ms <= SRT_QUEUE_REARM_MS
            && sample.srt.dropped_bytes == 0
            && sample.srt.retransmit_permille <= SRT_RETRANS_HEALTHY_PERMILLE;
        let srt_capacity = self.update_srt_capacity(&sample.srt, delivered_bps, current_media_bps);

        // A CC target is an estimate, while ACK-confirmed delivery is a lower
        // bound and a healthy SRT sender proves that its current media load is
        // not backing up. Never feed either proven load through the safety
        // margin a second time: doing so turns an 18 Mbps working stream into a
        // ~14 Mbps recommendation. This floor also prevents a link CC bootstrap
        // target (1 Mbps) from surfacing as ~670 kbps while the encoder is
        // demonstrably carrying many times that rate.
        let mut proven_load_bps = delivered_bps;
        if transport_healthy {
            proven_load_bps = proven_load_bps.max(current_media_bps);
        }
        let proven_capacity_floor =
            ((proven_load_bps as f64) / self.config.safety_margin).ceil() as u64;
        let effective_link_capacity = link_capacity.max(proven_capacity_floor);
        let capacity = srt_capacity
            .map(|srt| effective_link_capacity.min(srt))
            .unwrap_or(effective_link_capacity);
        let srt_limited = srt_capacity.is_some_and(|srt| srt < effective_link_capacity);
        let media_budget = ((capacity as f64) * self.config.safety_margin) as u64;
        let raw = media_budget.saturating_sub(sample.audio_bps);
        if sample.srt.ready
            && sample.srt.send_buffer_ms <= SRT_QUEUE_REARM_MS
            && sample.srt.dropped_bytes == 0
            && sample.srt.retransmit_permille <= SRT_RETRANS_HEALTHY_PERMILLE
        {
            self.srt_backoff_armed = true;
        }
        let transport_backoff = transport_stressed && self.srt_backoff_armed;
        if transport_backoff {
            self.srt_backoff_armed = false;
        }

        let mut recommended = if sample.all_links_down {
            // Keep the output alive without allowing an unbounded queue while
            // every path is unavailable.  The start bitrate is only used when
            // links are warming and no estimate exists yet.
            self.config.min_bps
        } else if !sample.capacity_ready {
            // Capacity is still warming, so there is no evidence that the
            // encoder's already-active rate is unsafe. Never turn a stale or
            // default start value into an immediate downshift.
            self.config.start_bps.max(current).min(self.config.max_bps)
        } else {
            raw.clamp(self.config.min_bps, self.config.max_bps)
        };
        if transport_backoff && !sample.all_links_down {
            let queue_safe = ((current as f64) * SRT_QUEUE_BACKOFF_FACTOR) as u64;
            recommended = recommended.min(queue_safe.max(self.config.min_bps));
        }
        let mut applied = current;
        let mut changed = false;
        let emergency = sample.all_links_down || transport_backoff;
        let first_sample = !self.has_sample;
        self.has_sample = true;
        let growth_frozen = self.growth_freeze_ticks > 0 || transport_stressed;
        if growth_frozen {
            self.growth_freeze_ticks = self.growth_freeze_ticks.saturating_sub(1);
        }

        if sample.all_links_down || (recommended as f64) < current as f64 * DOWN_DEADBAND {
            applied = recommended.min(current).max(self.config.min_bps);
            self.stable_headroom_ticks = 0;
            self.ticks_since_up = 0;
        } else if (recommended as f64) >= current as f64 * DEFAULT_UP_HEADROOM {
            if growth_frozen || first_sample {
                self.stable_headroom_ticks = 0;
                if first_sample {
                    self.ticks_since_up = 0;
                } else {
                    self.ticks_since_up = self.ticks_since_up.saturating_add(1);
                }
            } else {
                self.stable_headroom_ticks = self.stable_headroom_ticks.saturating_add(1);
                self.ticks_since_up = self.ticks_since_up.saturating_add(1);
                if self.stable_headroom_ticks >= UP_STABILITY_TICKS
                    && self.ticks_since_up >= UP_INTERVAL_TICKS
                {
                    let step = (current / 10).clamp(ROUNDING_BPS, MAX_UP_STEP_BPS);
                    applied = current.saturating_add(step).min(recommended);
                    self.ticks_since_up = 0;
                }
            }
        } else {
            self.stable_headroom_ticks = 0;
            self.ticks_since_up = self.ticks_since_up.saturating_add(1);
        }

        applied = round_bps(applied).clamp(self.config.min_bps, self.config.max_bps);
        if applied != current {
            changed = true;
            self.last_change_ms = Some(sample.now_ms);
        }

        AbrDecision {
            link_capacity_bps: link_capacity,
            srt_capacity_bps: srt_capacity.unwrap_or(0),
            estimated_capacity_bps: capacity,
            media_budget_bps: media_budget,
            recommended_bps: recommended,
            applied_bps: applied,
            changed,
            emergency,
            srt_limited,
            transport_stressed,
        }
    }
}

fn blend_bps(previous: u64, sample: u64, sample_weight: u64, total_weight: u64) -> u64 {
    let previous_weight = total_weight.saturating_sub(sample_weight);
    ((u128::from(previous) * u128::from(previous_weight)
        + u128::from(sample) * u128::from(sample_weight))
        / u128::from(total_weight)) as u64
}

fn srt_transport_stressed(srt: &SrtCapacity) -> bool {
    srt.send_buffer_ms >= SRT_QUEUE_STRESS_MS
        || srt.dropped_bytes > 0
        || (srt.send_buffer_ms > SRT_QUEUE_REARM_MS
            && srt.retransmit_permille >= SRT_RETRANS_STRESS_PERMILLE)
}

fn round_bps(value: u64) -> u64 {
    value / ROUNDING_BPS * ROUNDING_BPS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(capacity: u64, current: u64) -> AbrSample {
        AbrSample {
            links: vec![LinkCapacity {
                enabled: true,
                payload_eligible: true,
                capacity_ready: true,
                target_bps: capacity,
                delivered_bps: 0,
            }],
            current_video_bps: current,
            now_ms: 1_000,
            capacity_ready: true,
            ..Default::default()
        }
    }

    #[test]
    fn capacity_is_margin_adjusted() {
        let mut abr = AbrController::new(AbrConfig {
            max_bps: 10_000_000,
            ..AbrConfig::default()
        });
        let decision = abr.decide(&sample(10_000_000, 2_000_000));
        assert_eq!(decision.estimated_capacity_bps, 10_000_000);
        assert_eq!(decision.media_budget_bps, 8_000_000);
    }

    #[test]
    fn capacity_drop_reduces_immediately() {
        let mut abr = AbrController::new(AbrConfig {
            max_bps: 10_000_000,
            ..AbrConfig::default()
        });
        let _ = abr.decide(&sample(10_000_000, 8_000_000));
        let decision = abr.decide(&sample(2_000_000, 8_000_000));
        assert!(decision.changed);
        assert_eq!(decision.applied_bps, 1_600_000);
    }

    #[test]
    fn growth_waits_for_stability() {
        let mut abr = AbrController::new(AbrConfig {
            max_bps: 10_000_000,
            ..AbrConfig::default()
        });
        let initial = abr.decide(&sample(3_000_000, 1_500_000));
        assert!(!initial.changed);
        for _ in 0..9 {
            assert!(!abr.decide(&sample(10_000_000, 1_500_000)).changed);
        }
        assert!(abr.decide(&sample(10_000_000, 1_500_000)).changed);
    }

    #[test]
    fn disabled_links_do_not_contribute() {
        let mut abr = AbrController::new(AbrConfig {
            max_bps: 10_000_000,
            ..AbrConfig::default()
        });
        let mut s = sample(4_000_000, 1_500_000);
        s.links.push(LinkCapacity {
            enabled: false,
            payload_eligible: true,
            capacity_ready: true,
            target_bps: 100_000_000,
            delivered_bps: 0,
        });
        let d = abr.decide(&s);
        assert_eq!(d.estimated_capacity_bps, 4_000_000);
    }

    #[test]
    fn all_links_down_uses_start_bitrate_and_emergency_flag() {
        let mut abr = AbrController::new(AbrConfig {
            max_bps: 10_000_000,
            ..AbrConfig::default()
        });
        let mut s = sample(0, 5_000_000);
        s.capacity_ready = false;
        s.all_links_down = true;
        let d = abr.decide(&s);
        assert!(d.emergency);
        assert_eq!(d.applied_bps, 500_000);
    }

    #[test]
    fn link_warmup_does_not_reduce_an_active_encoder() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 1_500_000,
            max_bps: 30_000_000,
            ..AbrConfig::default()
        });
        let mut s = sample(1_000_000, 18_000_000);
        s.capacity_ready = false;
        s.links[0].capacity_ready = false;

        let decision = abr.decide(&s);

        assert_eq!(decision.recommended_bps, 18_000_000);
        assert_eq!(decision.applied_bps, 18_000_000);
        assert!(!decision.changed);
    }

    #[test]
    fn link_warmup_freezes_growth_but_not_reductions() {
        let mut abr = AbrController::new(AbrConfig {
            max_bps: 10_000_000,
            ..AbrConfig::default()
        });
        abr.freeze_growth_ticks(10);
        for _ in 0..10 {
            assert!(!abr.decide(&sample(10_000_000, 1_500_000)).changed);
        }
        assert!(!abr.decide(&sample(10_000_000, 1_500_000)).changed);
        let reduced = abr.decide(&sample(500_000, 1_500_000));
        assert!(reduced.changed);
        assert_eq!(reduced.applied_bps, 500_000);
    }

    #[test]
    fn stressed_srt_bandwidth_caps_the_link_sum() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 8_000_000,
            max_bps: 20_000_000,
            ..AbrConfig::default()
        });
        let mut decision = AbrDecision::default();
        for tick in 1..=SRT_STATS_WARMUP_TICKS {
            let mut s = sample(10_000_000, 8_000_000);
            s.now_ms = u64::from(tick) * 1_000;
            s.srt = SrtCapacity {
                ready: true,
                sampled_at_ms: s.now_ms,
                bandwidth_bps: 6_000_000,
                send_buffer_ms: SRT_QUEUE_STRESS_MS,
                ..Default::default()
            };
            decision = abr.decide(&s);
        }
        assert_eq!(decision.link_capacity_bps, 10_000_000);
        assert_eq!(decision.srt_capacity_bps, 6_000_000);
        assert_eq!(decision.estimated_capacity_bps, 6_000_000);
        assert_eq!(decision.applied_bps, 4_800_000);
        assert!(decision.srt_limited);
    }

    #[test]
    fn srt_capacity_above_link_sum_does_not_inflate_budget() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 8_000_000,
            max_bps: 20_000_000,
            ..AbrConfig::default()
        });
        let mut decision = AbrDecision::default();
        for tick in 1..=SRT_STATS_WARMUP_TICKS {
            let mut s = sample(10_000_000, 8_000_000);
            s.now_ms = u64::from(tick) * 1_000;
            s.srt = SrtCapacity {
                ready: true,
                sampled_at_ms: s.now_ms,
                bandwidth_bps: 30_000_000,
                ..Default::default()
            };
            decision = abr.decide(&s);
        }
        assert_eq!(decision.srt_capacity_bps, 30_000_000);
        assert_eq!(decision.estimated_capacity_bps, 10_000_000);
        assert!(!decision.srt_limited);
    }

    #[test]
    fn repeated_srt_snapshot_does_not_fake_warmup() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 8_000_000,
            max_bps: 20_000_000,
            ..AbrConfig::default()
        });
        let mut s = sample(10_000_000, 8_000_000);
        s.srt = SrtCapacity {
            ready: true,
            sampled_at_ms: 1_000,
            bandwidth_bps: 6_000_000,
            ..Default::default()
        };
        for tick in 1..=5 {
            s.now_ms = tick * 1_000;
            let decision = abr.decide(&s);
            assert_eq!(decision.srt_capacity_bps, 0);
            assert_eq!(decision.estimated_capacity_bps, 10_000_000);
        }
    }

    #[test]
    fn healthy_acknowledged_delivery_rejects_an_impossible_srt_floor() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 4_000_000,
            max_bps: 20_000_000,
            ..AbrConfig::default()
        });
        let mut decision = AbrDecision::default();
        for tick in 1..=SRT_STATS_WARMUP_TICKS {
            let mut s = sample(10_000_000, 4_000_000);
            s.links[0].delivered_bps = 4_000_000;
            s.now_ms = u64::from(tick) * 1_000;
            s.srt = SrtCapacity {
                ready: true,
                sampled_at_ms: s.now_ms,
                bandwidth_bps: 2_000_000,
                ..Default::default()
            };
            decision = abr.decide(&s);
        }
        assert_eq!(decision.srt_capacity_bps, 0);
        assert_eq!(decision.estimated_capacity_bps, 10_000_000);
        assert!(!decision.srt_limited);
    }

    #[test]
    fn healthy_load_following_srt_estimate_cannot_ratchet_video_down() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 8_000_000,
            max_bps: 20_000_000,
            ..AbrConfig::default()
        });
        let mut current = 8_000_000;
        for tick in 1..=20 {
            let mut s = sample(20_000_000, current);
            s.audio_bps = 128_000;
            s.links[0].delivered_bps = current.saturating_add(s.audio_bps);
            s.now_ms = tick * 1_000;
            s.srt = SrtCapacity {
                ready: true,
                sampled_at_ms: s.now_ms,
                // A load-following packet-pair estimate looks plausible because
                // it is above delivery, but its safety-adjusted value is lower
                // than the rate already being carried successfully.
                bandwidth_bps: s.links[0].delivered_bps * 11 / 10,
                // Mild retransmission without a backed-up sender queue is not
                // sufficient evidence to turn that estimate into a hard cap.
                retransmit_permille: 150,
                ..Default::default()
            };
            let decision = abr.decide(&s);
            assert_eq!(decision.srt_capacity_bps, 0);
            assert!(decision.applied_bps >= current);
            current = decision.applied_bps;
        }
        assert!(current >= 8_000_000);
    }

    #[test]
    fn healthy_eighteen_mbps_load_cannot_surface_bootstrap_recommendation() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 18_000_000,
            max_bps: 30_000_000,
            ..AbrConfig::default()
        });
        let mut decision = AbrDecision::default();
        for tick in 1..=SRT_STATS_WARMUP_TICKS {
            let mut s = sample(1_000_000, 18_000_000);
            s.audio_bps = 128_000;
            s.now_ms = u64::from(tick) * 1_000;
            s.srt = SrtCapacity {
                ready: true,
                sampled_at_ms: s.now_ms,
                bandwidth_bps: 30_000_000,
                ..Default::default()
            };
            decision = abr.decide(&s);
        }

        assert_eq!(decision.link_capacity_bps, 1_000_000);
        assert!(decision.estimated_capacity_bps >= 22_660_000);
        assert!(decision.recommended_bps >= 18_000_000);
        assert_eq!(decision.applied_bps, 18_000_000);
    }

    #[test]
    fn srt_queue_backoff_is_hysteretic_without_a_bandwidth_probe() {
        let mut abr = AbrController::new(AbrConfig {
            start_bps: 8_000_000,
            max_bps: 20_000_000,
            ..AbrConfig::default()
        });
        let mut stressed = sample(10_000_000, 8_000_000);
        stressed.srt = SrtCapacity {
            ready: true,
            sampled_at_ms: 1_000,
            bandwidth_bps: 0,
            send_buffer_ms: SRT_QUEUE_STRESS_MS,
            ..Default::default()
        };
        let first = abr.decide(&stressed);
        assert_eq!(first.applied_bps, 6_400_000);
        assert!(first.emergency);

        stressed.current_video_bps = first.applied_bps;
        stressed.now_ms = 2_000;
        stressed.srt.sampled_at_ms = 2_000;
        let held = abr.decide(&stressed);
        assert_eq!(held.applied_bps, first.applied_bps);
        assert!(!held.emergency);

        let mut recovered = stressed.clone();
        recovered.now_ms = 3_000;
        recovered.srt.sampled_at_ms = 3_000;
        recovered.srt.send_buffer_ms = 0;
        let recovered_decision = abr.decide(&recovered);
        assert_eq!(recovered_decision.applied_bps, first.applied_bps);

        stressed.current_video_bps = recovered_decision.applied_bps;
        stressed.now_ms = 4_000;
        stressed.srt.sampled_at_ms = 4_000;
        let second = abr.decide(&stressed);
        assert_eq!(second.applied_bps, 5_100_000);
        assert!(second.emergency);
    }
}
