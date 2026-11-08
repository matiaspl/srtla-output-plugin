//! Startup bandwidth probe.
//!
//! Before the encoder starts, the plugin sends MPEG-TS null packets
//! (PID 0x1FFF, discarded by every demuxer) over the connected SRT session at
//! a rate chosen here. The rate starts low to learn the idle RTT and queue
//! baselines, then doubles roughly every 350 ms until the path shows
//! congestion (RTT inflation, loss, or acknowledged rate falling behind the
//! offered rate) or the configured maximum is exceeded. The acknowledged
//! delivery rate at that point is the measured capacity; the encoder starts
//! below it and the ABR controller is seeded with it as its ceiling. The probe
//! then pauses so the queue it built drains before real media starts.

use crate::abr::{Estimator, SrtCapacity};

pub const DEFAULT_BUDGET_MS: u64 = 2_500;
const MIN_BUDGET_MS: u64 = 500;
const MAX_BUDGET_MS: u64 = 10_000;
const BASELINE_MS: u64 = 300;
const BASELINE_MIN_SAMPLES: usize = 5;
const RAMP_DOUBLING_MS: f64 = 350.0;
const RAMP_GRACE_MS: u64 = 150;
const HOLD_MS: u64 = 300;
const DRAIN_MAX_MS: u64 = 800;
const START_RATE_BPS: u64 = 1_000_000;
const TOP_HEADROOM_PERMILLE: u64 = 1_100;
const CAPACITY_WINDOW_MS: u64 = 200;
const BUILDING_WINDOW_MS: u64 = 300;
const LOSS_WINDOW_MS: u64 = 200;
const LOSS_LIMIT: f64 = 0.03;
const BUILDING_RATIO: f64 = 0.80;
const CONGESTION_HITS: u32 = 2;
/// Start the encoder this far below the measured capacity; the ABR
/// controller approaches the remaining headroom on its own.
const RESULT_FACTOR: f64 = 0.80;
const TIMEOUT_FACTOR: f64 = 0.90;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Baseline,
    Ramp,
    Hold,
    Drain,
    Done,
}

#[derive(Clone, Copy, Debug)]
pub struct ProbeConfig {
    pub min_video_bps: u64,
    pub max_video_bps: u64,
    pub audio_bps: u64,
    pub budget_ms: u64,
    /// Video bitrate to use when the probe produced no usable feedback.
    pub fallback_video_bps: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// Recommended starting video bitrate.
    pub video_bps: u64,
    /// Measured end-to-end capacity (all media) when the probe reached
    /// congestion; `None` when the configured maximum fit without congestion.
    pub capacity_bps: Option<u64>,
    /// `false` when no usable SRT feedback arrived and `video_bps` is the
    /// fallback.
    pub measured: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProbeStep {
    /// Null-packet pacing rate until the next step.
    pub send_rate_bps: u64,
    /// Set once the probe has finished, including the drain pause.
    pub outcome: Option<ProbeOutcome>,
}

#[derive(Clone, Debug)]
pub struct ProbeController {
    cfg: ProbeConfig,
    phase: Phase,
    started_ms: Option<u64>,
    phase_started_ms: u64,
    ramp_start_bps: u64,
    rate_bps: u64,
    estimator: Estimator,
    samples: usize,
    last_sample_ms: Option<u64>,
    best_goodput_bps: u64,
    sustained_bps: u64,
    congestion_hits: u32,
    outcome: Option<ProbeOutcome>,
}

impl ProbeController {
    pub fn new(cfg: ProbeConfig) -> Self {
        let mut cfg = cfg;
        cfg.max_video_bps = cfg.max_video_bps.max(cfg.min_video_bps).max(1);
        cfg.budget_ms = if cfg.budget_ms == 0 {
            DEFAULT_BUDGET_MS
        } else {
            cfg.budget_ms.clamp(MIN_BUDGET_MS, MAX_BUDGET_MS)
        };
        cfg.fallback_video_bps = cfg
            .fallback_video_bps
            .clamp(cfg.min_video_bps, cfg.max_video_bps);
        let start = START_RATE_BPS
            .max(cfg.min_video_bps + cfg.audio_bps)
            .min(cfg.max_video_bps + cfg.audio_bps);
        Self {
            cfg,
            phase: Phase::Baseline,
            started_ms: None,
            phase_started_ms: 0,
            ramp_start_bps: start,
            rate_bps: start,
            estimator: Estimator::default(),
            samples: 0,
            last_sample_ms: None,
            best_goodput_bps: 0,
            sustained_bps: 0,
            congestion_hits: 0,
            outcome: None,
        }
    }

    pub(crate) fn estimator(&self) -> &Estimator {
        &self.estimator
    }

    pub fn active(&self) -> bool {
        self.phase != Phase::Done
    }

    fn top_bps(&self) -> u64 {
        (self.cfg.max_video_bps + self.cfg.audio_bps) * TOP_HEADROOM_PERMILLE / 1_000
    }

    fn enter(&mut self, phase: Phase, now_ms: u64) {
        self.phase = phase;
        self.phase_started_ms = now_ms;
    }

    fn video_from_total(&self, total_bps: f64) -> u64 {
        (total_bps - self.cfg.audio_bps as f64)
            .max(0.0)
            .clamp(self.cfg.min_video_bps as f64, self.cfg.max_video_bps as f64) as u64
    }

    fn finish(&mut self, now_ms: u64, outcome: ProbeOutcome) {
        self.outcome = Some(outcome);
        if self.samples == 0 {
            self.enter(Phase::Done, now_ms);
        } else {
            self.enter(Phase::Drain, now_ms);
        }
    }

    fn finish_congested(&mut self, now_ms: u64) {
        let capacity = self.best_goodput_bps.max(self.sustained_bps);
        let outcome = ProbeOutcome {
            video_bps: self.video_from_total(capacity as f64 * RESULT_FACTOR),
            capacity_bps: Some(capacity),
            measured: true,
        };
        self.finish(now_ms, outcome);
    }

    fn finish_unconstrained(&mut self, now_ms: u64) {
        let outcome = ProbeOutcome {
            video_bps: self.cfg.max_video_bps,
            capacity_bps: None,
            measured: true,
        };
        self.finish(now_ms, outcome);
    }

    fn finish_timeout(&mut self, now_ms: u64) {
        let outcome = if self.samples < BASELINE_MIN_SAMPLES || self.sustained_bps == 0 {
            ProbeOutcome {
                video_bps: self.cfg.fallback_video_bps,
                capacity_bps: None,
                measured: false,
            }
        } else {
            // Everything up to the sustained rate fit; capacity is unknown.
            ProbeOutcome {
                video_bps: self.video_from_total(self.sustained_bps as f64 * TIMEOUT_FACTOR),
                capacity_bps: None,
                measured: true,
            }
        };
        self.finish(now_ms, outcome);
    }

    fn congested(&mut self, srt: &SrtCapacity) -> bool {
        let est = &self.estimator;
        let floor = est.rtt_floor_ms();
        let dev = est.rtt_deviation_ms();
        let rtt = f64::from(srt.rtt_ms);
        let rtt_up = rtt > floor + 30.0_f64.max(floor * 0.4).max(dev * 4.0);
        let loss = est.loss_ratio(LOSS_WINDOW_MS) > LOSS_LIMIT;
        let building = est
            .delivery(BUILDING_WINDOW_MS, floor as u64)
            .is_some_and(|d| d.offered_bps > self.ramp_start_bps && d.ratio() < BUILDING_RATIO);
        if let Some(d) = est.delivery(CAPACITY_WINDOW_MS, floor as u64) {
            self.best_goodput_bps = self.best_goodput_bps.max(d.goodput_bps);
            if !rtt_up && !loss && !building {
                self.sustained_bps = self.sustained_bps.max(d.goodput_bps);
            }
        }
        if rtt_up || loss || building {
            self.congestion_hits += 1;
        } else {
            self.congestion_hits = 0;
        }
        self.congestion_hits >= CONGESTION_HITS
    }

    pub fn step(&mut self, now_ms: u64, srt: &SrtCapacity) -> ProbeStep {
        let started = *self.started_ms.get_or_insert(now_ms);
        if self.phase == Phase::Done {
            return ProbeStep {
                send_rate_bps: 0,
                outcome: self.outcome,
            };
        }
        let fresh = srt.ready
            && srt.rtt_ms > 0
            && self
                .last_sample_ms
                .is_none_or(|last| srt.sampled_at_ms > last);
        if fresh {
            self.last_sample_ms = Some(srt.sampled_at_ms);
            self.estimator.push(now_ms, srt);
            self.samples += 1;
        }
        let elapsed = now_ms.saturating_sub(started);

        match self.phase {
            Phase::Baseline => {
                self.rate_bps = self.ramp_start_bps;
                if fresh && self.samples > BASELINE_MIN_SAMPLES {
                    // Baseline traffic may already exceed a tiny path.
                    if let Some(d) = self.estimator.delivery(CAPACITY_WINDOW_MS, 0) {
                        self.sustained_bps = self.sustained_bps.max(d.goodput_bps);
                    }
                }
                // Let the pipe fill (two RTTs) before judging delivery.
                let baseline_ms = BASELINE_MS.max(self.estimator.rtt_floor_ms() as u64 * 2);
                if elapsed >= baseline_ms && self.samples >= BASELINE_MIN_SAMPLES {
                    self.enter(Phase::Ramp, now_ms);
                }
            }
            Phase::Ramp | Phase::Hold => {
                let in_phase = now_ms.saturating_sub(self.phase_started_ms);
                if self.phase == Phase::Ramp {
                    let factor = 2f64.powf(in_phase as f64 / RAMP_DOUBLING_MS);
                    self.rate_bps =
                        ((self.ramp_start_bps as f64 * factor) as u64).min(self.top_bps());
                }
                let in_ramp_grace = self.phase == Phase::Ramp && in_phase < RAMP_GRACE_MS;
                if fresh && !in_ramp_grace && self.congested(srt) {
                    self.finish_congested(now_ms);
                } else if self.phase == Phase::Ramp && self.rate_bps >= self.top_bps() {
                    self.enter(Phase::Hold, now_ms);
                } else if self.phase == Phase::Hold && in_phase >= HOLD_MS {
                    self.finish_unconstrained(now_ms);
                }
            }
            Phase::Drain | Phase::Done => {}
        }

        if matches!(self.phase, Phase::Baseline | Phase::Ramp | Phase::Hold)
            && elapsed >= self.cfg.budget_ms
        {
            self.finish_timeout(now_ms);
        }

        if self.phase == Phase::Drain {
            self.rate_bps = 0;
            let floor = self.estimator.rtt_floor_ms();
            let drained = fresh
                && f64::from(srt.rtt_ms) <= floor + 15.0_f64.max(floor * 0.15)
                && self
                    .estimator
                    .queue_floor_ms()
                    .is_none_or(|q| f64::from(srt.send_buffer_ms) <= q + 20.0);
            if drained || now_ms.saturating_sub(self.phase_started_ms) >= DRAIN_MAX_MS {
                self.enter(Phase::Done, now_ms);
            }
        }

        ProbeStep {
            send_rate_bps: if self.phase == Phase::Done {
                0
            } else {
                self.rate_bps
            },
            outcome: if self.phase == Phase::Done {
                self.outcome
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abr::sim::{Sim, SimConfig};

    fn config(max_video_bps: u64) -> ProbeConfig {
        ProbeConfig {
            min_video_bps: 500_000,
            max_video_bps,
            audio_bps: 128_000,
            budget_ms: 0,
            fallback_video_bps: max_video_bps,
        }
    }

    /// Drive the probe against the simulated path; returns the outcome, the
    /// elapsed time, and the peak RTT seen.
    fn run(cfg: SimConfig, probe_cfg: ProbeConfig) -> (ProbeOutcome, u64, f64, ProbeController) {
        let mut sim = Sim::new(cfg);
        let mut probe = ProbeController::new(probe_cfg);
        let mut rate = 0u64;
        let mut peak_rtt = 0.0f64;
        let start = sim.now_ms;
        loop {
            let (stats, point) = sim.step(rate as f64);
            peak_rtt = peak_rtt.max(point.rtt_ms);
            let step = probe.step(sim.now_ms, &stats);
            if let Some(outcome) = step.outcome {
                return (outcome, sim.now_ms - start, peak_rtt, probe);
            }
            rate = step.send_rate_bps;
            assert!(sim.now_ms - start < 20_000, "probe did not finish");
        }
    }

    #[test]
    fn probe_finds_a_bottleneck_below_the_maximum() {
        for capacity in [2_000_000u64, 4_000_000, 8_000_000] {
            let (outcome, elapsed, peak_rtt, _) = run(
                SimConfig {
                    capacity: vec![(0, capacity)],
                    ..SimConfig::default()
                },
                config(20_000_000),
            );
            let measured = outcome.capacity_bps.expect("congestion reached");
            eprintln!(
                "capacity {capacity}: measured {measured} video {} in {elapsed} ms, peak RTT {peak_rtt:.0} ms",
                outcome.video_bps
            );
            assert!(outcome.measured);
            assert!(
                measured as f64 >= capacity as f64 * 0.75,
                "underestimate {measured}"
            );
            assert!(
                measured as f64 <= capacity as f64 * 1.10,
                "overestimate {measured}"
            );
            assert!(outcome.video_bps < capacity);
            assert!(elapsed <= DEFAULT_BUDGET_MS + DRAIN_MAX_MS);
        }
    }

    #[test]
    fn probe_stops_at_the_configured_maximum() {
        let (outcome, elapsed, _, _) = run(
            SimConfig {
                capacity: vec![(0, 50_000_000)],
                ..SimConfig::default()
            },
            config(6_000_000),
        );
        assert_eq!(outcome.video_bps, 6_000_000);
        assert_eq!(outcome.capacity_bps, None);
        assert!(outcome.measured);
        assert!(elapsed < DEFAULT_BUDGET_MS);
    }

    #[test]
    fn probe_without_feedback_uses_the_fallback() {
        let mut probe = ProbeController::new(config(6_000_000));
        let mut now = 0;
        let outcome = loop {
            now += 20;
            let step = probe.step(now, &SrtCapacity::default());
            if let Some(outcome) = step.outcome {
                break outcome;
            }
            assert!(now < 10_000);
        };
        assert!(!outcome.measured);
        assert_eq!(outcome.video_bps, 6_000_000);
        assert!(now >= DEFAULT_BUDGET_MS);
    }

    #[test]
    fn probe_handles_a_high_rtt_path() {
        let (outcome, _, _, _) = run(
            SimConfig {
                capacity: vec![(0, 5_000_000)],
                base_rtt_ms: 250.0,
                rtt_noise_ms: 20.0,
                ..SimConfig::default()
            },
            config(20_000_000),
        );
        let measured = outcome.capacity_bps.expect("congestion reached");
        assert!(
            (3_500_000..=5_500_000).contains(&measured),
            "measured {measured}"
        );
    }

    #[test]
    fn probe_seeds_a_controller_that_holds_near_capacity() {
        let sim_cfg = SimConfig {
            capacity: vec![(0, 5_000_000)],
            ..SimConfig::default()
        };
        let (outcome, elapsed, _, probe) = run(sim_cfg.clone(), config(20_000_000));
        let mut abr = crate::abr::sim::controller_for(&sim_cfg, outcome.video_bps);
        abr.seed_from_probe(
            elapsed,
            outcome.video_bps,
            outcome.capacity_bps.map(|c| c - 128_000),
            probe.estimator(),
        );
        let mut sim = Sim::new(sim_cfg.clone());
        sim.now_ms = elapsed;
        let trace = crate::abr::sim::run_abr(&mut abr, &mut sim, outcome.video_bps, 30_000);
        let m = crate::abr::sim::report(
            "probe-seeded 5M",
            &trace,
            elapsed,
            elapsed + 30_000,
            128_000,
        );
        assert!(
            m.min_target_bps >= 3_500_000,
            "dipped to {}",
            m.min_target_bps
        );
        assert!(m.peak_rtt_ms < 200.0, "peak RTT {}", m.peak_rtt_ms);
        assert!(m.reversals <= 6, "reversals {}", m.reversals);
    }
}
