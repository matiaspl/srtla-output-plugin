//! Deterministic closed-loop model used to evaluate the bitrate controller and
//! the startup probe. It is intentionally simple: one bottleneck with a
//! drop-tail buffer, SRT-style smoothed RTT, retransmission of lost packets,
//! and an encoder that follows its target with a first-order lag plus
//! per-frame rate noise. The aim is to compare control laws, not to predict
//! absolute numbers on a real network.

use super::*;

pub const TICK_MS: u64 = 20;
const PAYLOAD: f64 = 1_316.0;

#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Step schedule of bottleneck capacity: (from_ms, bps).
    pub capacity: Vec<(u64, u64)>,
    pub base_rtt_ms: f64,
    /// Optional step schedule of base RTT (from_ms, ms); overrides
    /// `base_rtt_ms` from each step on. Models handovers.
    pub base_rtt: Vec<(u64, f64)>,
    /// Peak-to-peak random RTT noise added to the bottleneck RTT.
    pub rtt_noise_ms: f64,
    /// Drop-tail buffer at the bottleneck, expressed as queueing time at the
    /// initial capacity.
    pub buffer_ms: f64,
    pub latency_ms: u32,
    pub audio_bps: u64,
    pub encoder_tau_ms: f64,
    /// Relative per-tick encoder output noise (0.15 = +/-15 %).
    pub encoder_noise: f64,
    pub min_bps: u64,
    pub max_bps: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            capacity: vec![(0, 8_000_000)],
            base_rtt_ms: 60.0,
            base_rtt: Vec::new(),
            rtt_noise_ms: 4.0,
            buffer_ms: 1_000.0,
            latency_ms: 2_000,
            audio_bps: 128_000,
            encoder_tau_ms: 400.0,
            encoder_noise: 0.15,
            min_bps: 500_000,
            max_bps: 20_000_000,
        }
    }
}

pub struct Sim {
    pub cfg: SimConfig,
    pub now_ms: u64,
    queue_bytes: f64,
    retransmit_backlog: f64,
    srtt: f64,
    rng: u64,
    buffer_bytes: f64,
    /// Bytes that left the bottleneck in each recent tick; they are
    /// acknowledged one base RTT later.
    served_ticks: std::collections::VecDeque<f64>,
    /// Bottleneck FIFO: (enqueue time, bytes). Queueing delay is the age of
    /// the oldest byte, so a stall delays packets by the stall's length
    /// rather than by "bytes / momentary capacity".
    fifo: std::collections::VecDeque<(u64, f64)>,
}

#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
pub struct TracePoint {
    pub t_ms: u64,
    pub target_bps: u64,
    pub encoder_bps: f64,
    pub capacity_bps: u64,
    pub rtt_ms: f64,
    pub queue_ms: f64,
    pub lost_bytes: f64,
    /// Approximate one-way delay (half base RTT plus bottleneck queueing).
    pub one_way_ms: f64,
}

impl Sim {
    pub fn new(cfg: SimConfig) -> Self {
        let first_capacity = cfg.capacity.first().map(|c| c.1).unwrap_or(1) as f64;
        Self {
            buffer_bytes: first_capacity / 8.0 * cfg.buffer_ms / 1_000.0,
            srtt: cfg.base_rtt_ms,
            cfg,
            now_ms: 0,
            queue_bytes: 0.0,
            retransmit_backlog: 0.0,
            rng: 0x2545_f491_4f6c_dd1d,
            served_ticks: std::collections::VecDeque::new(),
            fifo: std::collections::VecDeque::new(),
        }
    }

    pub fn capacity_at(&self, t_ms: u64) -> u64 {
        self.cfg
            .capacity
            .iter()
            .rev()
            .find(|(from, _)| *from <= t_ms)
            .map(|c| c.1)
            .unwrap_or(1)
    }

    pub fn base_rtt_at(&self, t_ms: u64) -> f64 {
        self.cfg
            .base_rtt
            .iter()
            .rev()
            .find(|(from, _)| *from <= t_ms)
            .map_or(self.cfg.base_rtt_ms, |b| b.1)
    }

    /// Uniform in [-1, 1).
    pub fn noise(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }

    /// Advance one tick with `offered_bps` of unique payload entering SRT and
    /// return the stats SRT would report at the end of the tick.
    pub fn step(&mut self, offered_bps: f64) -> (SrtCapacity, TracePoint) {
        self.now_ms += TICK_MS;
        let dt = TICK_MS as f64 / 1_000.0;
        let capacity = self.capacity_at(self.now_ms) as f64;
        let unique_bytes = offered_bps.max(0.0) / 8.0 * dt;
        let retransmit_bytes = self.retransmit_backlog.min(capacity / 8.0 * dt);
        self.retransmit_backlog -= retransmit_bytes;
        let arriving = unique_bytes + retransmit_bytes;
        let service = capacity / 8.0 * dt;
        let served = service.min(self.queue_bytes + arriving);
        self.queue_bytes = (self.queue_bytes + arriving - served).max(0.0);
        if arriving > 0.0 {
            self.fifo.push_back((self.now_ms, arriving));
        }
        let mut to_serve = served;
        while to_serve > 0.0 {
            let Some(front) = self.fifo.front_mut() else {
                break;
            };
            if front.1 <= to_serve {
                to_serve -= front.1;
                self.fifo.pop_front();
            } else {
                front.1 -= to_serve;
                to_serve = 0.0;
            }
        }
        self.served_ticks.push_back(served);
        let base_rtt = self.base_rtt_at(self.now_ms);
        let pipe_ticks = ((base_rtt / TICK_MS as f64).round() as usize).max(1);
        while self.served_ticks.len() > pipe_ticks {
            self.served_ticks.pop_front();
        }
        let mut lost = 0.0;
        if self.queue_bytes > self.buffer_bytes {
            lost = self.queue_bytes - self.buffer_bytes;
            self.queue_bytes = self.buffer_bytes;
            self.retransmit_backlog += lost;
            // Drop-tail: the newest bytes are discarded.
            let mut to_drop = lost;
            while to_drop > 0.0 {
                let Some(back) = self.fifo.back_mut() else {
                    break;
                };
                if back.1 <= to_drop {
                    to_drop -= back.1;
                    self.fifo.pop_back();
                } else {
                    back.1 -= to_drop;
                    to_drop = 0.0;
                }
            }
        }
        // Sojourn time of the head of the queue, plus this tick's service
        // time for what is still queued.
        let queue_ms = match self.fifo.front() {
            Some((t, _)) if self.queue_bytes > 1.0 => {
                (self.now_ms - t) as f64 + TICK_MS as f64 / 2.0
            }
            _ => 0.0,
        };
        let noise = self.noise() * self.cfg.rtt_noise_ms / 2.0;
        let rtt_true = base_rtt + queue_ms + noise;
        // SRT updates its smoothed RTT (7/8 EWMA) on every ACKACK, roughly
        // every 10 ms while data flows.
        for _ in 0..2 {
            self.srtt += (rtt_true - self.srtt) / 8.0;
        }
        let pipe_bytes: f64 = self.served_ticks.iter().sum();
        let flight = (pipe_bytes + self.queue_bytes + self.retransmit_backlog) / PAYLOAD;
        let stats = SrtCapacity {
            ready: true,
            sampled_at_ms: self.now_ms,
            bandwidth_bps: capacity as u64,
            send_rate_bps: ((arriving * 8.0) / dt) as u64,
            send_buffer_ms: (self.srtt + 10.0) as u32,
            send_buffer_packets: (flight + unique_bytes / PAYLOAD / 2.0) as u32,
            rtt_ms: self.srtt.max(1.0) as u32,
            latency_ms: self.cfg.latency_ms,
            retransmit_permille: 0,
            dropped_bytes: 0,
            sent_unique_bytes: unique_bytes as u64,
            packets_in_flight: flight as u32,
            sender_loss_packets: (lost / PAYLOAD) as u32,
        };
        let trace = TracePoint {
            t_ms: self.now_ms,
            target_bps: 0,
            encoder_bps: 0.0,
            capacity_bps: capacity as u64,
            rtt_ms: self.srtt,
            queue_ms,
            lost_bytes: lost,
            one_way_ms: base_rtt / 2.0 + queue_ms,
        };
        (stats, trace)
    }
}

pub fn sample_for(cfg: &SimConfig, now_ms: u64, current_bps: u64, srt: SrtCapacity) -> AbrSample {
    AbrSample {
        links: vec![LinkCapacity {
            enabled: true,
            payload_eligible: true,
            capacity_ready: true,
            target_bps: 0,
            delivered_bps: 0,
        }],
        audio_bps: cfg.audio_bps,
        current_video_bps: current_bps,
        now_ms,
        capacity_ready: true,
        all_links_down: false,
        srt,
    }
}

/// Run the ABR controller in closed loop for `duration_ms`.
pub fn run_abr(
    abr: &mut AbrController,
    sim: &mut Sim,
    start_bps: u64,
    duration_ms: u64,
) -> Vec<TracePoint> {
    let mut target = start_bps;
    let mut encoder = start_bps as f64;
    let mut trace = Vec::new();
    let end = sim.now_ms + duration_ms;
    while sim.now_ms < end {
        let alpha = (TICK_MS as f64 / sim.cfg.encoder_tau_ms).min(1.0);
        encoder += (target as f64 - encoder) * alpha;
        let jitter = 1.0 + sim.noise() * sim.cfg.encoder_noise;
        let offered = encoder * jitter + sim.cfg.audio_bps as f64;
        let (stats, mut point) = sim.step(offered);
        let cfg = sim.cfg.clone();
        let decision = abr.decide(&sample_for(&cfg, sim.now_ms, target, stats));
        target = decision.applied_bps;
        point.target_bps = target;
        point.encoder_bps = encoder;
        trace.push(point);
    }
    trace
}

pub struct Metrics {
    pub reversals: usize,
    pub changes: usize,
    pub utilization: f64,
    pub mean_rtt_ms: f64,
    pub peak_rtt_ms: f64,
    pub min_target_bps: u64,
    pub lost_bytes: f64,
}

pub fn metrics(trace: &[TracePoint], from_ms: u64, to_ms: u64, audio_bps: u64) -> Metrics {
    let window: Vec<_> = trace
        .iter()
        .filter(|p| p.t_ms >= from_ms && p.t_ms < to_ms)
        .collect();
    let mut reversals = 0;
    let mut changes = 0;
    let mut last_direction = 0i8;
    for pair in window.windows(2) {
        let direction = match pair[1].target_bps.cmp(&pair[0].target_bps) {
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
        };
        if direction != 0 {
            changes += 1;
            if last_direction != 0 && direction != last_direction {
                reversals += 1;
            }
            last_direction = direction;
        }
    }
    let n = window.len().max(1) as f64;
    Metrics {
        reversals,
        changes,
        utilization: window
            .iter()
            .map(|p| (p.encoder_bps + audio_bps as f64) / p.capacity_bps as f64)
            .sum::<f64>()
            / n,
        mean_rtt_ms: window.iter().map(|p| p.rtt_ms).sum::<f64>() / n,
        peak_rtt_ms: window.iter().map(|p| p.rtt_ms).fold(0.0, f64::max),
        min_target_bps: window.iter().map(|p| p.target_bps).min().unwrap_or(0),
        lost_bytes: window.iter().map(|p| p.lost_bytes).sum(),
    }
}

/// First time at or after `from_ms` when the target reaches `fraction` of
/// the video share of capacity (or `cap_bps`, whichever is lower).
pub fn time_to_reach(trace: &[TracePoint], from_ms: u64, bps: u64) -> Option<u64> {
    trace
        .iter()
        .find(|p| p.t_ms >= from_ms && p.target_bps >= bps)
        .map(|p| p.t_ms - from_ms)
}

pub fn controller_for(cfg: &SimConfig, start_bps: u64) -> AbrController {
    AbrController::new(AbrConfig {
        min_bps: cfg.min_bps,
        start_bps,
        max_bps: cfg.max_bps,
    })
}

pub fn report(
    name: &str,
    trace: &[TracePoint],
    from_ms: u64,
    to_ms: u64,
    audio_bps: u64,
) -> Metrics {
    let m = metrics(trace, from_ms, to_ms, audio_bps);
    eprintln!(
        "{name:<34} changes={:>4} reversals={:>4} util={:>5.2} rtt mean/peak={:>6.0}/{:>6.0} ms min_target={:>5} kb/s lost={:>8.0} B",
        m.changes,
        m.reversals,
        m.utilization,
        m.mean_rtt_ms,
        m.peak_rtt_ms,
        m.min_target_bps / 1_000,
        m.lost_bytes
    );
    m
}

#[test]
fn simulation_report() {
    // Steady 8 Mb/s bottleneck, controller allowed up to 20 Mb/s.
    let cfg = SimConfig::default();
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 6_000_000);
    let trace = run_abr(&mut abr, &mut sim, 6_000_000, 60_000);
    report("steady 8M (20-60 s)", &trace, 20_000, 60_000, cfg.audio_bps);

    // Capacity dip 8 -> 3 -> 8 Mb/s.
    let cfg = SimConfig {
        capacity: vec![(0, 8_000_000), (20_000, 3_000_000), (35_000, 8_000_000)],
        ..SimConfig::default()
    };
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 6_000_000);
    let trace = run_abr(&mut abr, &mut sim, 6_000_000, 70_000);
    report(
        "dip: during 3M (22-35 s)",
        &trace,
        22_000,
        35_000,
        cfg.audio_bps,
    );
    report(
        "dip: response (20-22 s)",
        &trace,
        20_000,
        22_000,
        cfg.audio_bps,
    );
    eprintln!(
        "dip: recovery to 6 Mb/s after 35 s: {:?} ms",
        time_to_reach(&trace, 35_000, 6_000_000)
    );

    // Recovery from the floor with plenty of headroom.
    let cfg = SimConfig {
        capacity: vec![(0, 12_000_000)],
        max_bps: 8_000_000,
        ..SimConfig::default()
    };
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 500_000);
    let trace = run_abr(&mut abr, &mut sim, 500_000, 40_000);
    eprintln!(
        "floor -> 7 Mb/s with 12M capacity: {:?} ms",
        time_to_reach(&trace, 0, 7_000_000)
    );

    // Noisy cellular-like path.
    let cfg = SimConfig {
        capacity: vec![(0, 5_000_000)],
        base_rtt_ms: 120.0,
        rtt_noise_ms: 30.0,
        buffer_ms: 2_000.0,
        ..SimConfig::default()
    };
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 6_000_000);
    let trace = run_abr(&mut abr, &mut sim, 6_000_000, 60_000);
    report(
        "cellular 5M noisy (20-60 s)",
        &trace,
        20_000,
        60_000,
        cfg.audio_bps,
    );

    // Near-outage: 8 -> 0.4 Mb/s for 3 s.
    let cfg = SimConfig {
        capacity: vec![(0, 8_000_000), (20_000, 400_000), (23_000, 8_000_000)],
        buffer_ms: 300.0,
        ..SimConfig::default()
    };
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 6_000_000);
    let trace = run_abr(&mut abr, &mut sim, 6_000_000, 50_000);
    report(
        "outage 0.4M (20-23 s)",
        &trace,
        20_000,
        23_000,
        cfg.audio_bps,
    );
    eprintln!(
        "outage: recovery to 6 Mb/s after 23 s: {:?} ms",
        time_to_reach(&trace, 23_000, 6_000_000)
    );
    report(
        "outage: after (30-50 s)",
        &trace,
        30_000,
        50_000,
        cfg.audio_bps,
    );

    // Slow fade 8 -> 4 Mb/s over 20 s.
    let cfg = SimConfig {
        capacity: (0..=20)
            .map(|i| (20_000 + i * 1_000, 8_000_000 - i * 200_000))
            .chain(std::iter::once((0, 8_000_000)))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
        ..SimConfig::default()
    };
    let mut cfg = cfg;
    cfg.capacity.sort();
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 6_000_000);
    let trace = run_abr(&mut abr, &mut sim, 6_000_000, 60_000);
    report(
        "fade 8->4M (20-40 s)",
        &trace,
        20_000,
        40_000,
        cfg.audio_bps,
    );
    report(
        "fade: settled 4M (40-60 s)",
        &trace,
        40_000,
        60_000,
        cfg.audio_bps,
    );

    // Shallow buffer: congestion shows up as loss more than delay.
    let cfg = SimConfig {
        capacity: vec![(0, 6_000_000)],
        buffer_ms: 80.0,
        ..SimConfig::default()
    };
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 8_000_000);
    let trace = run_abr(&mut abr, &mut sim, 8_000_000, 60_000);
    report(
        "shallow buffer 6M (20-60 s)",
        &trace,
        20_000,
        60_000,
        cfg.audio_bps,
    );
}

#[test]
#[ignore]
fn debug_trace() {
    let cfg = if std::env::var("STEADY").is_ok() {
        SimConfig::default()
    } else if std::env::var("CELL").is_ok() {
        SimConfig {
            capacity: vec![(0, 5_000_000)],
            base_rtt_ms: 120.0,
            rtt_noise_ms: 30.0,
            buffer_ms: 2_000.0,
            ..SimConfig::default()
        }
    } else if std::env::var("OUTAGE").is_ok() {
        SimConfig {
            capacity: vec![(0, 8_000_000), (20_000, 400_000), (23_000, 8_000_000)],
            buffer_ms: 300.0,
            ..SimConfig::default()
        }
    } else {
        SimConfig {
            capacity: vec![(0, 8_000_000), (20_000, 3_000_000), (35_000, 8_000_000)],
            ..SimConfig::default()
        }
    };
    let only_changes = std::env::var("CHANGES").is_ok();
    let mut last_state = String::new();
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 6_000_000);
    let from: u64 = std::env::var("FROM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(19_800);
    let to: u64 = std::env::var("TO")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(23_000);
    let mut target = 6_000_000u64;
    let mut encoder = target as f64;
    while sim.now_ms < to {
        encoder += (target as f64 - encoder) * (TICK_MS as f64 / sim.cfg.encoder_tau_ms);
        let offered =
            encoder * (1.0 + sim.noise() * sim.cfg.encoder_noise) + sim.cfg.audio_bps as f64;
        let (stats, point) = sim.step(offered);
        let d = abr.decide(&sample_for(&cfg, sim.now_ms, target, stats.clone()));
        target = d.applied_bps;
        let show = !only_changes || d.control_state != last_state;
        last_state = d.control_state.clone();
        if sim.now_ms >= from && show {
            eprintln!(
                "{:>6} cap={:>5} tgt={:>5} enc={:>5.0} rtt={:>4} q={:>4} flight={:>5} gp={:>5} ceil={:>5} floor={} inc<{} L/H={}/{} {}",
                sim.now_ms,
                point.capacity_bps / 1000,
                target / 1000,
                encoder / 1000.0,
                stats.rtt_ms,
                stats.send_buffer_ms,
                stats.packets_in_flight,
                d.srt_capacity_bps / 1000,
                d.estimated_capacity_bps / 1000,
                d.rtt_floor_ms,
                d.rtt_increase_below_ms,
                d.rtt_decrease_above_ms,
                d.rtt_heavy_above_ms,
                d.control_state
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Time-varying paths (mobile / satellite)
// ---------------------------------------------------------------------------

/// Small deterministic PRNG for scenario generation.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Approximately standard normal.
    pub fn normal(&mut self) -> f64 {
        (0..6).map(|_| self.unit()).sum::<f64>() * 2.0_f64.sqrt() - 6.0 * 2.0_f64.sqrt() / 2.0
    }
}

const STEP_MS: u64 = 250;

/// Capacity schedule (from_ms, bps) and base-RTT schedule (from_ms, ms).
pub type PathSchedule = (Vec<(u64, u64)>, Vec<(u64, f64)>);

/// One cellular modem on a moving vehicle: log-normal capacity random walk
/// around `mean_bps`, cell-edge fades, and handovers that briefly stall the
/// link and change the base RTT.
pub fn mobile_link(seed: u64, mean_bps: f64, duration_ms: u64) -> PathSchedule {
    let mut rng = Rng::new(seed);
    let mut capacity = Vec::new();
    let mut rtt = Vec::new();
    let mut log_c = mean_bps.ln();
    let mut fade_until = 0;
    let mut fade_depth = 1.0;
    let mut next_handover = 15_000 + (rng.unit() * 20_000.0) as u64;
    let mut handover_until = 0;
    let mut base = 50.0 + rng.unit() * 40.0;
    rtt.push((0, base));
    let mut t = 0;
    while t < duration_ms {
        // Mean-reverting random walk: ~25 % moves within a couple of seconds.
        log_c += (mean_bps.ln() - log_c) * 0.03 + rng.normal() * 0.07;
        log_c = log_c.clamp((mean_bps * 0.25).ln(), (mean_bps * 2.2).ln());
        if t >= fade_until && rng.unit() < 0.012 {
            fade_until = t + 1_000 + (rng.unit() * 2_500.0) as u64;
            fade_depth = 0.12 + rng.unit() * 0.25;
        }
        if t >= next_handover {
            handover_until = t + 250 + (rng.unit() * 300.0) as u64;
            next_handover = t + 15_000 + (rng.unit() * 25_000.0) as u64;
            base = 40.0 + rng.unit() * 80.0;
            rtt.push((handover_until, base));
        }
        let mut c = log_c.exp();
        if t < fade_until {
            c *= fade_depth;
        }
        if t < handover_until {
            c = 30_000.0;
        }
        capacity.push((t, c as u64));
        t += STEP_MS;
    }
    (capacity, rtt)
}

/// Sum several independently varying modems (SRTLA bonding of one carrier
/// per modem). The bond's RTT follows the first modem.
pub fn bonded_links(seed: u64, means: &[f64], duration_ms: u64) -> PathSchedule {
    let links: Vec<_> = means
        .iter()
        .enumerate()
        .map(|(i, m)| mobile_link(seed * 31 + i as u64, *m, duration_ms))
        .collect();
    let capacity = (0..links[0].0.len())
        .map(|i| (links[0].0[i].0, links.iter().map(|l| l.0[i].1).sum()))
        .collect();
    (capacity, links[0].1.clone())
}

/// Driving past a tower: smooth swing between `low` and `high` bps.
pub fn sinusoid(low: f64, high: f64, period_ms: f64, duration_ms: u64) -> Vec<(u64, u64)> {
    (0..duration_ms / STEP_MS)
        .map(|i| {
            let t = i * STEP_MS;
            let phase = (t as f64 / period_ms * std::f64::consts::TAU).sin();
            (t, (low + (high - low) * (phase + 1.0) / 2.0) as u64)
        })
        .collect()
}

/// LEO satellite: capacity wanders, a short outage and RTT change at every
/// 15 s satellite handover.
pub fn leo(seed: u64, duration_ms: u64) -> PathSchedule {
    let mut rng = Rng::new(seed);
    let mut capacity = Vec::new();
    let mut rtt = vec![(0, 45.0)];
    let mut c: f64 = 18e6;
    let mut t = 0;
    while t < duration_ms {
        c = (c * (1.0 + rng.normal() * 0.05)).clamp(8e6, 30e6);
        let in_slot = t % 15_000;
        let value = if in_slot < 200 { 50_000.0 } else { c };
        if in_slot == 0 && t > 0 {
            rtt.push((t + 200, 30.0 + rng.unit() * 40.0));
        }
        capacity.push((t, value as u64));
        t += STEP_MS;
    }
    (capacity, rtt)
}

pub struct MobilityMetrics {
    /// Mean video bitrate actually encoded, relative to an ideal controller
    /// running at 90 % of instantaneous capacity (clipped to the maximum).
    pub efficiency: f64,
    /// Time the one-way delay exceeded the SRT latency (receiver drops).
    pub glitch_ms: u64,
    pub mean_rtt_ms: f64,
    pub reversals: usize,
    pub mean_video_bps: f64,
}

pub fn mobility_metrics(cfg: &SimConfig, trace: &[TracePoint], from_ms: u64) -> MobilityMetrics {
    let window: Vec<_> = trace.iter().filter(|p| p.t_ms >= from_ms).collect();
    let n = window.len().max(1) as f64;
    let ideal: f64 = window
        .iter()
        .map(|p| {
            (p.capacity_bps as f64 * 0.9 - cfg.audio_bps as f64)
                .clamp(cfg.min_bps as f64, cfg.max_bps as f64)
        })
        .sum::<f64>()
        / n;
    let actual = window.iter().map(|p| p.encoder_bps).sum::<f64>() / n;
    let glitch_ms = window
        .iter()
        .filter(|p| p.one_way_ms > f64::from(cfg.latency_ms))
        .count() as u64
        * TICK_MS;
    let m = metrics(trace, from_ms, u64::MAX, cfg.audio_bps);
    MobilityMetrics {
        efficiency: actual / ideal,
        glitch_ms,
        mean_rtt_ms: m.mean_rtt_ms,
        reversals: m.reversals,
        mean_video_bps: actual,
    }
}

pub fn mobility_scenarios() -> Vec<(&'static str, SimConfig)> {
    const D: u64 = 180_000;
    let mut out = Vec::new();
    for seed in 1..=3u64 {
        let (capacity, base_rtt) = mobile_link(seed, 6e6, D);
        out.push((
            [
                "moto single modem #1",
                "moto single modem #2",
                "moto single modem #3",
            ][seed as usize - 1],
            SimConfig {
                capacity,
                base_rtt,
                rtt_noise_ms: 15.0,
                buffer_ms: 1_500.0,
                max_bps: 12_000_000,
                ..SimConfig::default()
            },
        ));
    }
    let (capacity, base_rtt) = bonded_links(7, &[4e6, 3e6, 3e6], D);
    out.push((
        "moto 3-modem bond",
        SimConfig {
            capacity,
            base_rtt,
            rtt_noise_ms: 15.0,
            buffer_ms: 1_500.0,
            max_bps: 15_000_000,
            ..SimConfig::default()
        },
    ));
    out.push((
        "drive-by tower 3-9M / 20 s",
        SimConfig {
            capacity: sinusoid(3e6, 9e6, 20_000.0, D),
            rtt_noise_ms: 8.0,
            max_bps: 12_000_000,
            ..SimConfig::default()
        },
    ));
    let (capacity, base_rtt) = leo(11, D);
    out.push((
        "LEO satellite, 15 s handovers",
        SimConfig {
            capacity,
            base_rtt,
            rtt_noise_ms: 10.0,
            buffer_ms: 300.0,
            max_bps: 15_000_000,
            ..SimConfig::default()
        },
    ));
    out
}

#[test]
fn mobility_report() {
    eprintln!(
        "{:<32} {:>6} {:>9} {:>9} {:>6} {:>9}",
        "scenario", "eff", "glitch", "rtt", "rev", "video"
    );
    for (name, cfg) in mobility_scenarios() {
        let mut sim = Sim::new(cfg.clone());
        let start = 4_000_000;
        let mut abr = controller_for(&cfg, start);
        let trace = run_abr(&mut abr, &mut sim, start, 180_000);
        let m = mobility_metrics(&cfg, &trace, 10_000);
        eprintln!(
            "{name:<32} {:>5.0}% {:>7} ms {:>6.0} ms {:>6} {:>6.2} Mb",
            m.efficiency * 100.0,
            m.glitch_ms,
            m.mean_rtt_ms,
            m.reversals,
            m.mean_video_bps / 1e6
        );
    }
}

#[test]
#[ignore]
fn mobility_trace() {
    let which: usize = std::env::var("SCENARIO")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let from: u64 = std::env::var("FROM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let to: u64 = std::env::var("TO")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60_000);
    let every: u64 = std::env::var("EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let (name, cfg) = mobility_scenarios().remove(which);
    eprintln!("{name}");
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 4_000_000);
    let mut target = 4_000_000u64;
    let mut encoder = target as f64;
    while sim.now_ms < to {
        encoder += (target as f64 - encoder) * (TICK_MS as f64 / sim.cfg.encoder_tau_ms);
        let offered =
            encoder * (1.0 + sim.noise() * sim.cfg.encoder_noise) + sim.cfg.audio_bps as f64;
        let (stats, point) = sim.step(offered);
        let d = abr.decide(&sample_for(&cfg, sim.now_ms, target, stats.clone()));
        target = d.applied_bps;
        if sim.now_ms >= from && sim.now_ms.is_multiple_of(every) {
            eprintln!(
                "{:>6} cap={:>6} tgt={:>6} enc={:>6.0} rtt={:>5} owd={:>5.0} gp={:>6} ceil={:>6} mem={:>6} floor={:>4} {}",
                sim.now_ms,
                point.capacity_bps / 1000,
                target / 1000,
                encoder / 1000.0,
                stats.rtt_ms,
                point.one_way_ms,
                d.srt_capacity_bps / 1000,
                d.estimated_capacity_bps / 1000,
                abr.capacity_memory_bps().unwrap_or(0) / 1000,
                d.rtt_floor_ms,
                d.control_state
            );
            if std::env::var("DEBUG").is_ok() {
                eprintln!("        {}", abr.debug_state());
            }
        }
    }
}
