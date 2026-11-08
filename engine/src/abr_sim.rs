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
        self.served_ticks.push_back(served);
        let pipe_ticks = ((self.cfg.base_rtt_ms / TICK_MS as f64).round() as usize).max(1);
        while self.served_ticks.len() > pipe_ticks {
            self.served_ticks.pop_front();
        }
        let mut lost = 0.0;
        if self.queue_bytes > self.buffer_bytes {
            lost = self.queue_bytes - self.buffer_bytes;
            self.queue_bytes = self.buffer_bytes;
            self.retransmit_backlog += lost;
        }
        let queue_ms = self.queue_bytes * 8.0 / capacity * 1_000.0;
        let noise = self.noise() * self.cfg.rtt_noise_ms / 2.0;
        let rtt_true = self.cfg.base_rtt_ms + queue_ms + noise;
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
