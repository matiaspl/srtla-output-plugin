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
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AbrSample {
    pub links: Vec<LinkCapacity>,
    pub audio_bps: u64,
    pub current_video_bps: u64,
    pub now_ms: u64,
    pub capacity_ready: bool,
    pub all_links_down: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AbrDecision {
    pub estimated_capacity_bps: u64,
    pub media_budget_bps: u64,
    pub recommended_bps: u64,
    pub applied_bps: u64,
    pub changed: bool,
    pub emergency: bool,
}

#[derive(Clone, Debug)]
pub struct AbrController {
    config: AbrConfig,
    stable_headroom_ticks: u32,
    ticks_since_up: u32,
    growth_freeze_ticks: u32,
    last_change_ms: Option<u64>,
    has_sample: bool,
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
        }
    }

    pub fn config(&self) -> &AbrConfig {
        &self.config
    }

    pub fn reset(&mut self) {
        self.stable_headroom_ticks = 0;
        self.ticks_since_up = UP_INTERVAL_TICKS;
        self.growth_freeze_ticks = 0;
        self.last_change_ms = None;
        self.has_sample = false;
    }

    /// Hold capacity increases for the warm-up period after a link is added
    /// or re-enabled.  Reductions remain active while the freeze is running.
    pub fn freeze_growth_ticks(&mut self, ticks: u32) {
        self.growth_freeze_ticks = self.growth_freeze_ticks.max(ticks);
        self.stable_headroom_ticks = 0;
    }

    pub fn decide(&mut self, sample: &AbrSample) -> AbrDecision {
        let capacity: u64 = sample
            .links
            .iter()
            .filter(|link| link.enabled && link.payload_eligible && link.capacity_ready)
            .map(|link| link.target_bps)
            .sum();
        let media_budget = ((capacity as f64) * self.config.safety_margin) as u64;
        let raw = media_budget.saturating_sub(sample.audio_bps);
        let recommended = if sample.all_links_down {
            // Keep the output alive without allowing an unbounded queue while
            // every path is unavailable.  The start bitrate is only used when
            // links are warming and no estimate exists yet.
            self.config.min_bps
        } else if !sample.capacity_ready {
            self.config.start_bps
        } else {
            raw.clamp(self.config.min_bps, self.config.max_bps)
        };

        let current = sample
            .current_video_bps
            .clamp(self.config.min_bps, self.config.max_bps);
        let mut applied = current;
        let mut changed = false;
        let mut emergency = sample.all_links_down;
        let first_sample = !self.has_sample;
        self.has_sample = true;
        let growth_frozen = self.growth_freeze_ticks > 0;
        if growth_frozen {
            self.growth_freeze_ticks -= 1;
        }

        if sample.all_links_down || (recommended as f64) < current as f64 * DOWN_DEADBAND {
            applied = recommended.min(current).max(self.config.min_bps);
            self.stable_headroom_ticks = 0;
            self.ticks_since_up = 0;
            emergency = sample.all_links_down;
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
            estimated_capacity_bps: capacity,
            media_budget_bps: media_budget,
            recommended_bps: recommended,
            applied_bps: applied,
            changed,
            emergency,
        }
    }
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
}
