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
    /// Optional step schedule of RTT noise (from_ms, ms); overrides
    /// `rtt_noise_ms` from each step on.
    pub rtt_noise: Vec<(u64, f64)>,
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
    /// Two-stage queue model (opt-in): the sending host's queue (socket send
    /// buffer, interface queue) holds at most this many bytes and feeds the
    /// modem; when the modem buffer is full it back-pressures the host, and
    /// only a full host queue drops (newest first). Lost packets are
    /// retransmitted about one RTT later with their original timestamp, and
    /// given up once older than the latency, as SRT does. `None` keeps the
    /// single drop-tail bottleneck used by the other scenarios.
    pub host_queue_bytes: Option<f64>,
    /// Modem buffer for the two-stage model, in bytes.
    pub modem_buffer_bytes: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            capacity: vec![(0, 8_000_000)],
            base_rtt_ms: 60.0,
            base_rtt: Vec::new(),
            rtt_noise_ms: 4.0,
            rtt_noise: Vec::new(),
            buffer_ms: 1_000.0,
            latency_ms: 2_000,
            audio_bps: 128_000,
            encoder_tau_ms: 400.0,
            encoder_noise: 0.15,
            min_bps: 500_000,
            max_bps: 20_000_000,
            host_queue_bytes: None,
            modem_buffer_bytes: 256.0 * 1024.0,
        }
    }
}

/// Media in flight in the two-stage model.
#[derive(Clone, Copy, Debug)]
struct Chunk {
    /// When the media was first sent (retransmissions keep it).
    media_ms: u64,
    /// When this copy entered the host queue.
    enqueued_ms: u64,
    bytes: f64,
}

#[derive(Default)]
struct TwoStage {
    host: std::collections::VecDeque<Chunk>,
    host_bytes: f64,
    modem: std::collections::VecDeque<Chunk>,
    modem_bytes: f64,
    retransmit: std::collections::VecDeque<(u64, Chunk)>,
    /// 100 ms slices of media (by send time) that arrived late or never.
    bad_slices: std::collections::BTreeSet<u64>,
    produced_bytes: f64,
    on_time_bytes: f64,
    late_bytes: f64,
    abandoned_bytes: f64,
}

/// Delivery quality of the two-stage model over a window of media time.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeliveryStats {
    pub glitch_ms: u64,
    pub on_time_fraction: f64,
    pub late_bytes: f64,
    pub abandoned_bytes: f64,
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
    two_stage: TwoStage,
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
            two_stage: TwoStage::default(),
        }
    }

    /// Late or lost media in the two-stage model, for media sent in
    /// [from_ms, to_ms).
    pub fn delivery_stats(&self, from_ms: u64, to_ms: u64) -> DeliveryStats {
        let t = &self.two_stage;
        let slices = t
            .bad_slices
            .range(from_ms / 100..to_ms.div_ceil(100))
            .count() as u64;
        let delivered = t.on_time_bytes + t.late_bytes + t.abandoned_bytes;
        DeliveryStats {
            glitch_ms: slices * 100,
            on_time_fraction: if delivered > 0.0 {
                t.on_time_bytes / delivered
            } else {
                1.0
            },
            late_bytes: t.late_bytes,
            abandoned_bytes: t.abandoned_bytes,
        }
    }

    fn step_two_stage(&mut self, offered_bps: f64, host_cap: f64) -> (SrtCapacity, TracePoint) {
        self.now_ms += TICK_MS;
        let now = self.now_ms;
        let dt = TICK_MS as f64 / 1_000.0;
        let capacity = self.capacity_at(now) as f64;
        let base_rtt = self.base_rtt_at(now);
        let latency = u64::from(self.cfg.latency_ms);
        let modem_cap = self.cfg.modem_buffer_bytes;
        let srtt = self.srtt;
        let t = &mut self.two_stage;

        // Retransmissions that are due; SRT gives up on data past the latency.
        let mut retransmit_bytes = 0.0;
        while t.retransmit.front().is_some_and(|(due, _)| *due <= now) {
            let (_, chunk) = t.retransmit.pop_front().expect("front");
            if now.saturating_sub(chunk.media_ms) > latency {
                t.abandoned_bytes += chunk.bytes;
                t.bad_slices.insert(chunk.media_ms / 100);
            } else {
                retransmit_bytes += chunk.bytes;
                t.host_bytes += chunk.bytes;
                t.host.push_back(Chunk {
                    enqueued_ms: now,
                    ..chunk
                });
            }
        }
        // New media.
        let unique_bytes = offered_bps.max(0.0) / 8.0 * dt;
        if unique_bytes > 0.0 {
            t.produced_bytes += unique_bytes;
            t.host_bytes += unique_bytes;
            t.host.push_back(Chunk {
                media_ms: now,
                enqueued_ms: now,
                bytes: unique_bytes,
            });
        }
        // The modem sends what its grant allows.
        let mut to_serve = capacity / 8.0 * dt;
        let mut served = 0.0;
        while to_serve > 0.0 {
            let Some(front) = t.modem.front_mut() else {
                break;
            };
            let part = front.bytes.min(to_serve);
            let arrival = now as f64 + base_rtt / 2.0;
            if arrival - front.media_ms as f64 <= latency as f64 {
                t.on_time_bytes += part;
            } else {
                t.late_bytes += part;
                t.bad_slices.insert(front.media_ms / 100);
            }
            front.bytes -= part;
            to_serve -= part;
            served += part;
            t.modem_bytes -= part;
            if front.bytes <= 1e-9 {
                t.modem.pop_front();
            }
        }
        // Host -> modem until the modem is full (USB back-pressure).
        while t.modem_bytes < modem_cap {
            let Some(front) = t.host.front_mut() else {
                break;
            };
            let part = front.bytes.min(modem_cap - t.modem_bytes);
            let mut moved = *front;
            moved.bytes = part;
            t.modem.push_back(moved);
            t.modem_bytes += part;
            t.host_bytes -= part;
            front.bytes -= part;
            if front.bytes <= 1e-9 {
                t.host.pop_front();
            }
        }
        // A full host queue drops the newest packets; SRT learns of the loss
        // about one RTT later and resends them.
        let mut lost = 0.0;
        while t.host_bytes > host_cap {
            let Some(back) = t.host.back_mut() else {
                break;
            };
            let part = back.bytes.min(t.host_bytes - host_cap);
            let mut dropped = *back;
            dropped.bytes = part;
            back.bytes -= part;
            t.host_bytes -= part;
            lost += part;
            if back.bytes <= 1e-9 {
                t.host.pop_back();
            }
            let due = now + srtt.max(base_rtt) as u64;
            t.retransmit.push_back((due, dropped));
        }
        t.retransmit.make_contiguous().sort_by_key(|(due, _)| *due);

        let oldest_queued = t
            .modem
            .front()
            .map(|c| c.enqueued_ms)
            .into_iter()
            .chain(t.host.front().map(|c| c.enqueued_ms))
            .min();
        let queue_ms = oldest_queued.map_or(0.0, |e| (now - e) as f64 + TICK_MS as f64 / 2.0);
        // SRT's send buffer spans the oldest unacknowledged media, including
        // data waiting to be resent.
        let oldest_unacked = t
            .modem
            .front()
            .map(|c| c.media_ms)
            .into_iter()
            .chain(t.host.iter().map(|c| c.media_ms))
            .chain(t.retransmit.iter().map(|(_, c)| c.media_ms))
            .min();
        let retransmit_pending: f64 = t.retransmit.iter().map(|(_, c)| c.bytes).sum();
        let queued = t.host_bytes + t.modem_bytes + retransmit_pending;

        self.served_ticks.push_back(served);
        let pipe_ticks = ((base_rtt / TICK_MS as f64).round() as usize).max(1);
        while self.served_ticks.len() > pipe_ticks {
            self.served_ticks.pop_front();
        }
        let noise_amplitude = self
            .cfg
            .rtt_noise
            .iter()
            .rev()
            .find(|(from, _)| *from <= now)
            .map_or(self.cfg.rtt_noise_ms, |n| n.1);
        let noise = self.noise() * noise_amplitude / 2.0;
        let rtt_true = base_rtt + queue_ms + noise;
        for _ in 0..2 {
            self.srtt += (rtt_true - self.srtt) / 8.0;
        }
        let pipe_bytes: f64 = self.served_ticks.iter().sum();
        let flight = (pipe_bytes + queued) / PAYLOAD;
        let send_buffer_ms =
            (self.srtt + 10.0).max(oldest_unacked.map_or(0.0, |m| (now - m) as f64 + 10.0));
        let stats = SrtCapacity {
            ready: true,
            sampled_at_ms: now,
            bandwidth_bps: capacity as u64,
            send_rate_bps: (((unique_bytes + retransmit_bytes) * 8.0) / dt) as u64,
            send_buffer_ms: send_buffer_ms as u32,
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
            t_ms: now,
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
        if let Some(host_cap) = self.cfg.host_queue_bytes {
            return self.step_two_stage(offered_bps, host_cap);
        }
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
        let noise_amplitude = self
            .cfg
            .rtt_noise
            .iter()
            .rev()
            .find(|(from, _)| *from <= self.now_ms)
            .map_or(self.cfg.rtt_noise_ms, |n| n.1);
        let noise = self.noise() * noise_amplitude / 2.0;
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
    run_system(abr, sim, start_bps, duration_ms, false)
}

const KEYFRAME_INTERVAL_MS: u64 = 2_000;

/// Closed loop with, optionally, the output's video shedding: a tick's video
/// is offered only if the shedder admits it (a keyframe every 2 s); audio is
/// always sent. With shedding, the trace's encoder rate is the video
/// actually sent.
pub fn run_system(
    abr: &mut AbrController,
    sim: &mut Sim,
    start_bps: u64,
    duration_ms: u64,
    shed: bool,
) -> Vec<TracePoint> {
    let mut target = start_bps;
    let mut encoder = start_bps as f64;
    let mut trace = Vec::new();
    let mut shedder = crate::shed::VideoShedder::default();
    let mut shed_estimator = Estimator::default();
    let mut last_stats: Option<SrtCapacity> = None;
    let end = sim.now_ms + duration_ms;
    while sim.now_ms < end {
        let alpha = (TICK_MS as f64 / sim.cfg.encoder_tau_ms).min(1.0);
        encoder += (target as f64 - encoder) * alpha;
        let jitter = 1.0 + sim.noise() * sim.cfg.encoder_noise;
        let mut admitted = true;
        if shed {
            let keyframe = (sim.now_ms + TICK_MS).is_multiple_of(KEYFRAME_INTERVAL_MS);
            let path = last_stats.as_ref().map(|srt| {
                let floor = shed_estimator.rtt_floor_ms();
                crate::shed::PathState {
                    queue_ms: f64::from(srt.send_buffer_ms),
                    rtt_floor_ms: floor,
                    latency_ms: f64::from(srt.latency_ms),
                    delivery_bps: shed_estimator
                        .delivery(500, floor as u64)
                        .map(|d| d.goodput_bps),
                }
            });
            let bytes = (encoder / 8.0 * TICK_MS as f64 / 1_000.0) as usize;
            let priority = if keyframe {
                crate::shed::PRIORITY_HIGHEST
            } else {
                2
            };
            admitted = shedder.admit(path.as_ref(), bytes, keyframe, priority);
        }
        let video = if admitted { encoder * jitter } else { 0.0 };
        let offered = video + sim.cfg.audio_bps as f64;
        let (stats, mut point) = sim.step(offered);
        if shed {
            shed_estimator.push(sim.now_ms, &stats);
            last_stats = Some(stats.clone());
        }
        let cfg = sim.cfg.clone();
        let decision = abr.decide(&sample_for(&cfg, sim.now_ms, target, stats));
        target = decision.applied_bps;
        point.target_bps = target;
        point.encoder_bps = if admitted { encoder } else { 0.0 };
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

// ---------------------------------------------------------------------------
// Radio access technology switching (5G <-> LTE <-> HSPA+)
// ---------------------------------------------------------------------------

/// Uplink characteristics of one radio access technology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rat {
    Nr5g,
    Lte,
    HspaPlus,
}

impl Rat {
    pub fn name(self) -> &'static str {
        match self {
            Rat::Nr5g => "5G",
            Rat::Lte => "LTE",
            Rat::HspaPlus => "HSPA+",
        }
    }

    /// Typical mean uplink throughput (mid-band NSA 5G, LTE, HSUPA).
    pub fn mean_bps(self) -> f64 {
        match self {
            Rat::Nr5g => 25e6,
            Rat::Lte => 7e6,
            Rat::HspaPlus => 2e6,
        }
    }

    pub fn base_rtt_ms(self) -> f64 {
        match self {
            Rat::Nr5g => 25.0,
            Rat::Lte => 45.0,
            Rat::HspaPlus => 110.0,
        }
    }

    pub fn rtt_noise_ms(self) -> f64 {
        match self {
            Rat::Nr5g => 6.0,
            Rat::Lte => 12.0,
            Rat::HspaPlus => 30.0,
        }
    }

    /// Inter-RAT transition stall: 5G NSA leg add/release is quick; moving
    /// to or from 3G is a redirect/reselection and stalls for ~1 s.
    pub fn stall_ms_to(self, to: Rat) -> u64 {
        match (self, to) {
            (Rat::Nr5g, Rat::Lte) => 150,
            (Rat::Lte, Rat::Nr5g) => 100,
            (Rat::Lte, Rat::HspaPlus) => 1_200,
            (Rat::HspaPlus, Rat::Lte) => 800,
            (Rat::Nr5g, Rat::HspaPlus) => 1_500,
            (Rat::HspaPlus, Rat::Nr5g) => 900,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RatPhase {
    pub rat: Rat,
    pub from: Option<Rat>,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Scripted back-and-forth: step-downs, step-ups, direct 5G <-> HSPA+
/// jumps, a 5 s 5G/LTE ping-pong and a short LTE -> HSPA+ -> LTE dip.
pub fn rat_sequence() -> Vec<(Rat, u64)> {
    use Rat::*;
    let mut seq = vec![
        (Nr5g, 30_000),
        (Lte, 25_000),
        (HspaPlus, 25_000),
        (Lte, 20_000),
        (Nr5g, 25_000),
        (HspaPlus, 20_000),
        (Nr5g, 20_000),
    ];
    for i in 0..8 {
        seq.push((if i % 2 == 0 { Lte } else { Nr5g }, 5_000));
    }
    seq.extend([
        (Lte, 10_000),
        (HspaPlus, 10_000),
        (Lte, 10_000),
        (Nr5g, 25_000),
    ]);
    seq
}

/// Build the path for a RAT sequence: per-RAT capacity random walk
/// (+/- ~15 %), base RTT and RTT noise, and a stall at every transition.
pub fn rat_switching(seed: u64, sequence: &[(Rat, u64)]) -> (SimConfig, Vec<RatPhase>) {
    let mut rng = Rng::new(seed);
    let mut capacity = Vec::new();
    let mut base_rtt = Vec::new();
    let mut rtt_noise = Vec::new();
    let mut phases = Vec::new();
    let mut t = 0;
    let mut previous: Option<Rat> = None;
    for &(rat, duration) in sequence {
        let start = t;
        let stall = previous.map_or(0, |p| p.stall_ms_to(rat));
        if stall > 0 {
            capacity.push((t, 30_000));
        }
        let live = t + stall;
        base_rtt.push((live, rat.base_rtt_ms()));
        rtt_noise.push((live, rat.rtt_noise_ms()));
        let mean = rat.mean_bps();
        let mut log_c = mean.ln();
        let mut step = live;
        while step < start + duration {
            log_c += (mean.ln() - log_c) * 0.1 + rng.normal() * 0.04;
            log_c = log_c.clamp((mean * 0.7).ln(), (mean * 1.35).ln());
            capacity.push((step, log_c.exp() as u64));
            step += STEP_MS;
        }
        phases.push(RatPhase {
            rat,
            from: previous,
            start_ms: start,
            end_ms: start + duration,
        });
        previous = Some(rat);
        t = start + duration;
    }
    let cfg = SimConfig {
        capacity,
        base_rtt,
        rtt_noise,
        base_rtt_ms: sequence[0].0.base_rtt_ms(),
        rtt_noise_ms: sequence[0].0.rtt_noise_ms(),
        // A modem buffer of fixed size: 400 ms at 5G rates is several
        // seconds on HSPA+, as on real 3G bufferbloat.
        buffer_ms: 400.0,
        latency_ms: 2_000,
        max_bps: 15_000_000,
        ..SimConfig::default()
    };
    (cfg, phases)
}

#[derive(Clone, Copy, Debug)]
pub struct PhaseMetrics {
    pub phase: RatPhase,
    /// Mean encoded video relative to 0.9 x capacity (clipped to the max).
    pub efficiency: f64,
    pub mean_video_bps: f64,
    pub mean_capacity_bps: f64,
    /// Time from the switch until the target is adapted: for a step down,
    /// at or below 110 % of the new phase's ideal; for a step up, at least
    /// 80 % of it. `None` if never within the phase.
    pub adapt_ms: Option<u64>,
    pub peak_one_way_ms: f64,
    pub glitch_ms: u64,
    pub reversals: usize,
}

pub fn phase_metrics(
    cfg: &SimConfig,
    trace: &[TracePoint],
    phases: &[RatPhase],
) -> Vec<PhaseMetrics> {
    let ideal_of =
        |cap: f64| (cap * 0.9 - cfg.audio_bps as f64).clamp(cfg.min_bps as f64, cfg.max_bps as f64);
    phases
        .iter()
        .map(|phase| {
            let window: Vec<_> = trace
                .iter()
                .filter(|p| p.t_ms > phase.start_ms && p.t_ms <= phase.end_ms)
                .collect();
            let n = window.len().max(1) as f64;
            // Exclude the transition stall from the capacity averages.
            let live: Vec<_> = window.iter().filter(|p| p.capacity_bps > 100_000).collect();
            let ln = live.len().max(1) as f64;
            let mean_capacity = live.iter().map(|p| p.capacity_bps as f64).sum::<f64>() / ln;
            let ideal = live
                .iter()
                .map(|p| ideal_of(p.capacity_bps as f64))
                .sum::<f64>()
                / ln;
            let mean_video = window.iter().map(|p| p.encoder_bps).sum::<f64>() / n;
            let stepping_up = phase
                .from
                .is_none_or(|f| f.mean_bps() < phase.rat.mean_bps());
            let adapt_ms = window
                .iter()
                .find(|p| {
                    if stepping_up {
                        p.target_bps as f64 >= ideal * 0.8
                    } else {
                        p.target_bps as f64 <= ideal * 1.1
                    }
                })
                .map(|p| p.t_ms - phase.start_ms);
            let m = metrics(trace, phase.start_ms, phase.end_ms, cfg.audio_bps);
            PhaseMetrics {
                phase: *phase,
                efficiency: mean_video / ideal,
                mean_video_bps: mean_video,
                mean_capacity_bps: mean_capacity,
                adapt_ms,
                peak_one_way_ms: window.iter().map(|p| p.one_way_ms).fold(0.0, f64::max),
                glitch_ms: window
                    .iter()
                    .filter(|p| p.one_way_ms > f64::from(cfg.latency_ms))
                    .count() as u64
                    * TICK_MS,
                reversals: m.reversals,
            }
        })
        .collect()
}

pub fn run_rat_switching(seed: u64, shed: bool) -> (SimConfig, Vec<PhaseMetrics>) {
    let (cfg, phases) = rat_switching(seed, &rat_sequence());
    let duration = phases.last().map_or(0, |p| p.end_ms);
    let mut sim = Sim::new(cfg.clone());
    let start = 6_000_000;
    let mut abr = controller_for(&cfg, start);
    let trace = run_system(&mut abr, &mut sim, start, duration, shed);
    let metrics = phase_metrics(&cfg, &trace, &phases);
    (cfg, metrics)
}

#[test]
fn rat_switching_report() {
    for shed in [false, true] {
        eprintln!("-- video shedding {}", if shed { "on" } else { "off" });
        rat_switching_table(&run_rat_switching(5, shed).1);
    }
}

fn rat_switching_table(metrics: &[PhaseMetrics]) {
    eprintln!(
        "{:>7} {:<15} {:>8} {:>8} {:>5} {:>9} {:>9} {:>7} {:>4}",
        "start", "switch", "cap", "video", "eff", "adapt", "peak owd", "glitch", "rev"
    );
    for m in metrics {
        let switch = match m.phase.from {
            Some(from) => format!("{}->{}", from.name(), m.phase.rat.name()),
            None => m.phase.rat.name().to_string(),
        };
        eprintln!(
            "{:>6.0}s {:<15} {:>5.1} Mb {:>5.1} Mb {:>4.0}% {:>9} {:>6.0} ms {:>4} ms {:>4}",
            m.phase.start_ms as f64 / 1000.0,
            switch,
            m.mean_capacity_bps / 1e6,
            m.mean_video_bps / 1e6,
            m.efficiency * 100.0,
            m.adapt_ms
                .map_or("never".to_string(), |a| format!("{a} ms")),
            m.peak_one_way_ms,
            m.glitch_ms,
            m.reversals
        );
    }
}

#[test]
#[ignore]
fn rat_switching_trace() {
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
        .unwrap_or(250);
    let (cfg, _) = rat_switching(5, &rat_sequence());
    let mut sim = Sim::new(cfg.clone());
    let mut abr = controller_for(&cfg, 6_000_000);
    let mut target = 6_000_000u64;
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
                "{:>6} cap={:>6} tgt={:>6} rtt={:>5} owd={:>5.0} gp={:>6} ceil={:>6} mem={:>6} floor={:>4} {}",
                sim.now_ms,
                point.capacity_bps / 1000,
                target / 1000,
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

#[test]
#[ignore]
fn rat_switching_seeds() {
    for seed in 1..=8 {
        let (_, m) = run_rat_switching(seed, true);
        let worst = |f: &dyn Fn(&PhaseMetrics) -> bool, g: &dyn Fn(&PhaseMetrics) -> u64| {
            m.iter().filter(|p| f(p)).map(g).max().unwrap_or(0)
        };
        let is = |from: Rat, to: Rat| {
            move |p: &PhaseMetrics| p.phase.from == Some(from) && p.phase.rat == to
        };
        let adapt = |p: &PhaseMetrics| p.adapt_ms.unwrap_or(99_999);
        let glitch = |p: &PhaseMetrics| p.glitch_ms;
        let min_eff = m
            .iter()
            .filter(|p| p.phase.end_ms - p.phase.start_ms >= 20_000)
            .map(|p| p.efficiency)
            .fold(f64::INFINITY, f64::min);
        eprintln!(
            "seed {seed}: down adapt {:>4} | 5G->LTE glitch {:>4} | LTE->5G adapt {:>5} | HSPA->LTE {:>5} | HSPA->5G {:>5} | LTE->HSPA glitch {:>5} | 5G->HSPA glitch {:>5} | min eff (long) {:.2} | max rev {}",
            worst(
                &|p| p
                    .phase
                    .from
                    .is_some_and(|f| f.mean_bps() > p.phase.rat.mean_bps()),
                &adapt
            ),
            worst(&is(Rat::Nr5g, Rat::Lte), &glitch),
            worst(&is(Rat::Lte, Rat::Nr5g), &adapt),
            worst(&is(Rat::HspaPlus, Rat::Lte), &adapt),
            worst(&is(Rat::HspaPlus, Rat::Nr5g), &adapt),
            worst(&is(Rat::Lte, Rat::HspaPlus), &glitch),
            worst(&is(Rat::Nr5g, Rat::HspaPlus), &glitch),
            min_eff,
            m.iter().map(|p| p.reversals).max().unwrap_or(0)
        );
    }
}

/// Scenarios for the host-queue experiment, with their durations.
pub fn queue_experiment_scenarios() -> Vec<(&'static str, SimConfig, u64)> {
    let mut out = Vec::new();
    let (cfg, phases) = rat_switching(5, &rat_sequence());
    out.push((
        "5G/LTE/HSPA+ switching",
        cfg,
        phases.last().map_or(0, |p| p.end_ms),
    ));
    for (name, cfg) in mobility_scenarios().into_iter().take(3) {
        out.push((name, cfg, 180_000));
    }
    out.push(("steady 8M", SimConfig::default(), 60_000));
    out.push((
        "dip 8->3->8M",
        SimConfig {
            capacity: vec![(0, 8_000_000), (20_000, 3_000_000), (35_000, 8_000_000)],
            ..SimConfig::default()
        },
        70_000,
    ));
    out
}

pub fn run_queue_experiment(
    cfg: &SimConfig,
    duration_ms: u64,
    host_queue_bytes: f64,
    modem_buffer_bytes: f64,
) -> (DeliveryStats, f64) {
    let cfg = SimConfig {
        host_queue_bytes: Some(host_queue_bytes),
        modem_buffer_bytes,
        ..cfg.clone()
    };
    let mut sim = Sim::new(cfg.clone());
    let start = 4_000_000;
    let mut abr = controller_for(&cfg, start);
    let trace = run_system(&mut abr, &mut sim, start, duration_ms, true);
    // Skip the first 10 s (start-up), and media too recent to have settled.
    let stats = sim.delivery_stats(10_000, duration_ms.saturating_sub(3_000));
    let window: Vec<_> = trace.iter().filter(|p| p.t_ms >= 10_000).collect();
    let mean_video = window.iter().map(|p| p.encoder_bps).sum::<f64>() / window.len().max(1) as f64;
    (stats, mean_video)
}

pub const HOST_QUEUES: [(&str, f64); 4] = [
    ("1.3 MB (txqueuelen 1000)", 1_000.0 * 1_316.0),
    ("256 KB", 256.0 * 1024.0),
    ("120 KB (Linux default)", 120.0 * 1024.0),
    ("32 KB", 32.0 * 1024.0),
];

#[test]
#[ignore]
fn queue_experiment_report() {
    for (modem_name, modem) in [("256 KB", 256.0 * 1024.0), ("1 MB", 1024.0 * 1024.0)] {
        eprintln!("== modem buffer {modem_name}");
        eprintln!(
            "{:<26} {:<26} {:>9} {:>8} {:>8} {:>8} {:>10}",
            "scenario", "host queue", "glitch", "on-time", "video", "late", "abandoned"
        );
        for (name, cfg, duration) in queue_experiment_scenarios() {
            for (host_name, host) in HOST_QUEUES {
                let (stats, video) = run_queue_experiment(&cfg, duration, host, modem);
                eprintln!(
                    "{name:<26} {host_name:<26} {:>6} ms {:>7.2}% {:>5.2} Mb {:>5.0} KB {:>7.0} KB",
                    stats.glitch_ms,
                    stats.on_time_fraction * 100.0,
                    video / 1e6,
                    stats.late_bytes / 1024.0,
                    stats.abandoned_bytes / 1024.0
                );
            }
        }
    }
}
