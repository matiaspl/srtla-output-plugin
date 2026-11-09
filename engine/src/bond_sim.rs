//! Multi-link (bonded) simulation driving the real SRTLA scheduler.
//!
//! The single-path simulator in `abr::sim` treats a bond as one aggregate
//! bottleneck. Real bonds behave differently: every packet is assigned to one
//! link by the SRTLA scheduler, links have their own queues, RTTs and
//! handovers, the receiver reorders across links, and a stalled link is taken
//! out of rotation by stall gating. This model keeps all of that:
//!
//! - **Scheduler:** the vendored `srtla-core` connections, link selection
//!   (enhanced mode, stall gating, quality, per-link congestion control),
//!   batching, stall probes, keepalive RTT, window recovery and phases are
//!   called directly on a simulated clock, in the order the embedded sender
//!   (`srtla_send::embedded`) calls them: per packet selection, a 15 ms batch
//!   flush, and 1 s housekeeping followed by the link CC tick. ACK/NAK
//!   handling mirrors `process_connection_events` (crate-private upstream).
//! - **Links:** per-link capacity, base RTT and jitter schedules; a host queue
//!   (socket send buffer + interface queue) feeding the modem buffer, which
//!   back-pressures the host when full. The embedded sender awaits a full
//!   socket, so a full host queue on *any* link blocks the whole sender loop
//!   until it drains.
//! - **SRT:** sequence numbers, a cumulative ACK every 10 ms over the link that
//!   last delivered data, ACKACK RTT (routed through the scheduler like any
//!   control packet), NAKs held for the measured cross-link transit spread
//!   (the SRTLA-patched receiver's reorder hold), retransmissions flagged so
//!   the scheduler steers them to the best link, and both ends giving up on
//!   data older than the latency. SRTLA ACKs every 10 packets per link.
//! - **Scoring:** media slices (100 ms) late or never delivered, on-time
//!   fraction, per-link share, retransmissions and time blocked.
//!
//! The encoder, bitrate controller and video shedding are the same as in the
//! single-path simulator.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use srtla_core::ConfigSnapshot;
use srtla_core::connection::SrtlaConnection;
use srtla_core::selection::link_cc::{CcState, LinkCcController};
use srtla_core::selection::select_connection_idx;
use srtla_send::sender::{SequenceTracker, recover_connection};

use crate::abr::sim::{Rng, TICK_MS as ABR_TICK_MS, controller_for, sample_for};
use crate::abr::sim::{SimConfig, TracePoint};
use crate::abr::{AbrController, AbrSample, Estimator, LinkCapacity, SrtCapacity};
use crate::shed::{PRIORITY_HIGHEST, PathState, VideoShedder};

/// Simulation step. The ABR samples every `ABR_TICK_MS` (20 ms).
const STEP_MS: u64 = 5;
const SRT_PAYLOAD: usize = 1_316;
const SRT_HEADER: usize = 16;
const SRT_DATA_BYTES: usize = SRT_PAYLOAD + SRT_HEADER;
const ACKACK_BYTES: usize = 44;
const SRT_ACK_INTERVAL_MS: u64 = 10;
const SRTLA_ACK_EVERY: usize = 10;
const BATCH_FLUSH_MS: u64 = 15;
const HOUSEKEEPING_MS: u64 = 1_000;
const KEYFRAME_INTERVAL_MS: u64 = 2_000;

#[derive(Clone, Debug)]
pub struct LinkConfig {
    pub name: &'static str,
    pub capacity: Vec<(u64, u64)>,
    pub base_rtt: Vec<(u64, f64)>,
    pub rtt_noise_ms: f64,
    /// Socket send buffer + interface queue, bytes. A full host queue
    /// blocks the sender loop.
    pub host_queue_bytes: f64,
    pub modem_buffer_bytes: f64,
}

impl LinkConfig {
    pub fn steady(name: &'static str, bps: u64, rtt_ms: f64) -> Self {
        Self {
            name,
            capacity: vec![(0, bps)],
            base_rtt: vec![(0, rtt_ms)],
            rtt_noise_ms: 8.0,
            host_queue_bytes: 1_000.0 * 1_316.0,
            modem_buffer_bytes: 256.0 * 1024.0,
        }
    }

    fn capacity_at(&self, t: u64) -> u64 {
        self.capacity
            .iter()
            .rev()
            .find(|(from, _)| *from <= t)
            .map_or(0, |c| c.1)
    }

    fn base_rtt_at(&self, t: u64) -> f64 {
        self.base_rtt
            .iter()
            .rev()
            .find(|(from, _)| *from <= t)
            .map_or(50.0, |c| c.1)
    }
}

#[derive(Clone, Debug)]
pub struct BondConfig {
    pub links: Vec<LinkConfig>,
    pub latency_ms: u32,
    pub audio_bps: u64,
    pub min_bps: u64,
    pub max_bps: u64,
    pub encoder_tau_ms: f64,
    pub encoder_noise: f64,
    pub shed: bool,
    /// Receiver SRTLA ACK cadence: always after `SRTLA_ACK_EVERY` packets on
    /// a link; with `Some(ms)`, also whatever is pending `ms` after the first
    /// unacknowledged packet (a time-based flush). `None` is the classic
    /// BELABOX `srtla_rec` behaviour.
    pub srtla_ack_flush_ms: Option<u64>,
}

impl BondConfig {
    pub fn new(links: Vec<LinkConfig>) -> Self {
        Self {
            links,
            latency_ms: 2_000,
            audio_bps: 128_000,
            min_bps: 500_000,
            max_bps: 15_000_000,
            encoder_tau_ms: 400.0,
            encoder_noise: 0.15,
            shed: true,
            srtla_ack_flush_ms: None,
        }
    }

    pub fn with_host_queue(mut self, bytes: f64) -> Self {
        for l in &mut self.links {
            l.host_queue_bytes = bytes;
        }
        self
    }

    fn abr_config(&self) -> SimConfig {
        SimConfig {
            latency_ms: self.latency_ms,
            audio_bps: self.audio_bps,
            min_bps: self.min_bps,
            max_bps: self.max_bps,
            encoder_tau_ms: self.encoder_tau_ms,
            encoder_noise: self.encoder_noise,
            ..SimConfig::default()
        }
    }
}

/// What travels up a link.
#[derive(Clone, Debug)]
enum Up {
    Data {
        seq: u32,
        media_ms: u64,
        sent_ms: u64,
        retransmit: bool,
    },
    AckAck {
        ack_sent_ms: u64,
    },
    Keepalive(Vec<u8>),
    Reg2,
}

#[derive(Clone, Debug)]
struct WirePacket {
    up: Up,
    bytes: usize,
    /// When it entered the host queue (for queueing delay).
    enq_ms: u64,
}

/// What travels back down a link to the sender.
#[derive(Clone, Debug)]
enum Down {
    SrtAck { ack: u32, rtt_ms: f64, sent_ms: u64 },
    SrtNak(Vec<u32>),
    SrtlaAck(Vec<u32>),
    KeepaliveEcho(Vec<u8>),
    Reg3,
}

struct LinkState {
    cfg: LinkConfig,
    host: VecDeque<WirePacket>,
    host_bytes: f64,
    modem: VecDeque<WirePacket>,
    modem_bytes: f64,
    credit: f64,
    /// (arrival at receiver, packet).
    up_flight: VecDeque<(u64, WirePacket)>,
    /// (arrival at sender, packet).
    down_flight: VecDeque<(u64, Down)>,
    /// Receiver-side SRTLA ACK accumulator for this link.
    srtla_ack_pending: Vec<u32>,
    srtla_ack_first_ms: u64,
    awaiting_reg3: bool,
    carried_bytes: f64,
    max_queue_ms: f64,
    rng: Rng,
}

impl LinkState {
    /// Waiting time of the oldest packet still queued on this link.
    fn queue_delay_ms(&self, now: u64) -> f64 {
        self.modem
            .front()
            .or(self.host.front())
            .map_or(0.0, |w| now.saturating_sub(w.enq_ms) as f64)
    }
}

struct SentPacket {
    media_ms: u64,
}

struct ReceiverState {
    /// Next sequence the receiver still needs (cumulative ACK point).
    next_needed: u32,
    max_seen: Option<u32>,
    received: BTreeSet<u32>,
    /// Missing sequences: (detected, last NAK).
    missing: BTreeMap<u32, (u64, Option<u64>)>,
    /// (arrival, transit) of recent first-transmission packets.
    transit: VecDeque<(u64, f64)>,
    last_active_link: usize,
    last_ack_ms: u64,
    srtt: f64,
}

#[derive(Clone, Debug, Default)]
pub struct BondReport {
    pub glitch_ms: u64,
    pub on_time_fraction: f64,
    pub mean_video_bps: f64,
    pub link_share: Vec<f64>,
    pub retransmit_fraction: f64,
    pub blocked_ms: u64,
    pub max_link_queue_ms: Vec<f64>,
    pub gate_events: Vec<u64>,
}

pub struct BondSim {
    cfg: BondConfig,
    base: u64,
    now: u64,
    links: Vec<LinkState>,
    conns: Vec<SrtlaConnection>,
    seq_tracker: SequenceTracker,
    snapshot: ConfigSnapshot,
    link_cc: LinkCcController,
    last_selected: Option<usize>,
    /// Wire packets in each connection's batch, in the order `take_batch`
    /// drains them.
    pending_wire: HashMap<usize, VecDeque<WirePacket>>,
    /// Per-link statistics as the SRTLA sender publishes them (refreshed by
    /// housekeeping once a second), which the engine hands to the ABR.
    published_links: Vec<LinkCapacity>,
    /// SRT packets waiting for the SRTLA sender (it may be blocked).
    srtla_input: VecDeque<WirePacket>,
    /// Link whose full host queue the sender loop is blocked on, with the
    /// batch it is trying to send.
    blocked: Option<(usize, Vec<WirePacket>)>,
    blocked_ms: u64,
    // SRT sender.
    next_seq: u32,
    unacked: BTreeMap<u32, SentPacket>,
    sender_srtt: f64,
    payload_credit: f64,
    sent_unique_bytes: u64,
    nak_count: u32,
    retransmitted: u64,
    produced_packets: u64,
    // SRT receiver.
    rx: ReceiverState,
    // Scoring (media time, ms).
    bad_slices: BTreeSet<u64>,
    on_time_packets: u64,
    late_or_lost_packets: u64,
    media_of: HashMap<u32, u64>,
}

impl BondSim {
    pub fn new(cfg: BondConfig) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let conns: Vec<SrtlaConnection> = runtime
            .block_on(srtla_core::test_helpers::create_test_connections(
                cfg.links.len(),
            ))
            .into_iter()
            .collect();
        let base = srtla_core::now_ms();
        let snapshot = ConfigSnapshot {
            negotiated_latency_ms: cfg.latency_ms,
            ..ConfigSnapshot::default()
        };
        let links = cfg
            .links
            .iter()
            .enumerate()
            .map(|(i, l)| LinkState {
                cfg: l.clone(),
                host: VecDeque::new(),
                host_bytes: 0.0,
                modem: VecDeque::new(),
                modem_bytes: 0.0,
                credit: 0.0,
                up_flight: VecDeque::new(),
                down_flight: VecDeque::new(),
                srtla_ack_pending: Vec::new(),
                srtla_ack_first_ms: 0,
                awaiting_reg3: false,
                carried_bytes: 0.0,
                max_queue_ms: 0.0,
                rng: Rng::new(17 + i as u64),
            })
            .collect();
        Self {
            base,
            now: 0,
            links,
            conns,
            seq_tracker: SequenceTracker::new(),
            snapshot,
            link_cc: LinkCcController::new(),
            last_selected: None,
            pending_wire: HashMap::new(),
            published_links: Vec::new(),
            srtla_input: VecDeque::new(),
            blocked: None,
            blocked_ms: 0,
            next_seq: 1_000,
            unacked: BTreeMap::new(),
            sender_srtt: 100.0,
            payload_credit: 0.0,
            sent_unique_bytes: 0,
            nak_count: 0,
            retransmitted: 0,
            produced_packets: 0,
            rx: ReceiverState {
                next_needed: 1_000,
                max_seen: None,
                received: BTreeSet::new(),
                missing: BTreeMap::new(),
                transit: VecDeque::new(),
                last_active_link: 0,
                last_ack_ms: 0,
                srtt: 100.0,
            },
            bad_slices: BTreeSet::new(),
            on_time_packets: 0,
            late_or_lost_packets: 0,
            media_of: HashMap::new(),
            cfg,
        }
    }

    /// Scheduler clock: same epoch as the connections were built on.
    fn clock(&self) -> u64 {
        self.base + self.now
    }

    // ---------------------------------------------------------------- SRT

    fn data_bytes(seq: u32, retransmit: bool) -> Vec<u8> {
        let mut pkt = vec![0u8; SRT_DATA_BYTES];
        pkt[0..4].copy_from_slice(&(seq & 0x7fff_ffff).to_be_bytes());
        if retransmit {
            srtla_protocol::set_srt_data_retransmit(&mut pkt);
        }
        pkt
    }

    /// The encoder hands `bytes` of new media to SRT this step.
    fn srt_send_media(&mut self, bytes: f64) {
        self.payload_credit += bytes;
        while self.payload_credit >= SRT_PAYLOAD as f64 {
            self.payload_credit -= SRT_PAYLOAD as f64;
            let seq = self.next_seq;
            self.next_seq += 1;
            self.unacked.insert(seq, SentPacket { media_ms: self.now });
            self.media_of.insert(seq, self.now);
            self.sent_unique_bytes += SRT_PAYLOAD as u64;
            self.produced_packets += 1;
            self.srtla_input.push_back(WirePacket {
                up: Up::Data {
                    seq,
                    media_ms: self.now,
                    sent_ms: self.now,
                    retransmit: false,
                },
                bytes: SRT_DATA_BYTES,
                enq_ms: 0,
            });
        }
    }

    fn srt_on_ack(&mut self, ack: u32, rtt_ms: f64, ack_sent_ms: u64) {
        let keep = self.unacked.split_off(&ack);
        self.unacked = keep;
        self.sender_srtt = rtt_ms;
        // ACKACK goes up through SRTLA like any control packet.
        self.srtla_input.push_back(WirePacket {
            up: Up::AckAck { ack_sent_ms },
            bytes: ACKACK_BYTES,
            enq_ms: 0,
        });
    }

    fn srt_on_nak(&mut self, seqs: &[u32]) {
        let latency = u64::from(self.cfg.latency_ms);
        for &seq in seqs {
            self.nak_count += 1;
            let Some(p) = self.unacked.get(&seq) else {
                continue;
            };
            if self.now.saturating_sub(p.media_ms) > latency {
                continue;
            }
            self.retransmitted += 1;
            // Retransmissions go ahead of new data.
            self.srtla_input.push_front(WirePacket {
                up: Up::Data {
                    seq,
                    media_ms: p.media_ms,
                    sent_ms: self.now,
                    retransmit: true,
                },
                bytes: SRT_DATA_BYTES,
                enq_ms: 0,
            });
        }
    }

    fn srt_sender_drop_old(&mut self) {
        let limit = u64::from(self.cfg.latency_ms) + 500;
        let now = self.now;
        self.unacked
            .retain(|_, p| now.saturating_sub(p.media_ms) <= limit);
    }

    fn srt_stats(&mut self) -> SrtCapacity {
        let oldest = self.unacked.values().map(|p| p.media_ms).min();
        let stats = SrtCapacity {
            ready: true,
            sampled_at_ms: self.now,
            bandwidth_bps: 0,
            send_rate_bps: self.sent_unique_bytes * 8_000 / ABR_TICK_MS,
            send_buffer_ms: oldest.map_or(0, |o| (self.now - o) as u32),
            send_buffer_packets: self.unacked.len() as u32,
            rtt_ms: self.sender_srtt.max(1.0) as u32,
            latency_ms: self.cfg.latency_ms,
            retransmit_permille: 0,
            dropped_bytes: 0,
            sent_unique_bytes: self.sent_unique_bytes,
            packets_in_flight: self.unacked.len() as u32,
            sender_loss_packets: self.nak_count,
        };
        self.sent_unique_bytes = 0;
        self.nak_count = 0;
        stats
    }

    // --------------------------------------------------------------- SRTLA

    /// Mirror of `handle_srt_datagram` + `forward_via_connection`. Returns
    /// false if the sender loop blocked.
    fn srtla_forward(&mut self, packet: WirePacket) -> bool {
        let t = self.clock();
        let (seq, retransmit, raw) = match &packet.up {
            Up::Data {
                seq, retransmit, ..
            } => (Some(*seq), *retransmit, Self::data_bytes(*seq, *retransmit)),
            _ => {
                let mut raw = vec![0u8; packet.bytes];
                raw[0] = 0x80;
                raw[1] = 0x06;
                (None, false, raw)
            }
        };
        let mut sel = select_connection_idx(&mut self.conns, self.last_selected, t, &self.snapshot);
        if seq.is_some()
            && retransmit
            && let Some(best) = srtla_core::priority::select_best_quality_idx(&self.conns, t)
        {
            sel = Some(best);
        }
        let Some(sel) = sel else {
            return true;
        };
        self.last_selected = Some(sel);
        let conn_id = self.conns[sel].conn_id;
        let needs_flush = self.conns[sel].queue_data_packet(&raw, seq, t);
        self.pending_wire.entry(sel).or_default().push_back(packet);
        if let Some(s) = seq {
            self.seq_tracker.insert_with_size(s, conn_id, t, raw.len());
        }
        if needs_flush && !self.flush_link(sel) {
            return false;
        }
        if let Some(s) = seq {
            self.send_stall_probes(sel, s, &raw);
        }
        true
    }

    fn send_stall_probes(&mut self, sel: usize, seq: u32, raw: &[u8]) {
        let t = self.clock();
        for i in 0..self.conns.len() {
            if i == sel {
                continue;
            }
            let c = &mut self.conns[i];
            if !c.admin_enabled || !(c.is_stall_gated() || c.is_quality_excluded()) || !c.connected
            {
                continue;
            }
            if !c.stall_probe_due() {
                continue;
            }
            let mut probe = raw.to_vec();
            srtla_protocol::set_srt_data_retransmit(&mut probe);
            let media_ms = self.media_of.get(&seq).copied().unwrap_or(self.now);
            let needs_flush = self.conns[i].queue_probe_packet(&probe, seq, t);
            self.pending_wire
                .entry(i)
                .or_default()
                .push_back(WirePacket {
                    up: Up::Data {
                        seq,
                        media_ms,
                        sent_ms: self.now,
                        retransmit: true,
                    },
                    bytes: probe.len(),
                    enq_ms: 0,
                });
            if needs_flush {
                let _ = self.flush_link(i);
            }
        }
    }

    /// Mirror of `send_connection_batch`: drain the connection's batch and
    /// hand it to the link's host queue. If it does not fit, the sender loop
    /// blocks until it does (the real code awaits the socket).
    fn flush_link(&mut self, idx: usize) -> bool {
        let t = self.clock();
        let batch = self.conns[idx].take_batch(t);
        if batch.is_empty() {
            return true;
        }
        let pending = self.pending_wire.entry(idx).or_default();
        let wire: Vec<WirePacket> = (0..batch.len())
            .filter_map(|_| pending.pop_front())
            .collect();
        self.conns[idx].note_sent(t);
        self.push_host(idx, wire)
    }

    fn push_host(&mut self, idx: usize, wire: Vec<WirePacket>) -> bool {
        let bytes: f64 = wire.iter().map(|w| w.bytes as f64).sum();
        let link = &mut self.links[idx];
        if link.host_bytes + bytes > link.cfg.host_queue_bytes && link.host_bytes > 0.0 {
            self.blocked = Some((idx, wire));
            return false;
        }
        for mut w in wire {
            w.enq_ms = self.now;
            link.host_bytes += w.bytes as f64;
            link.host.push_back(w);
        }
        true
    }

    fn flush_all(&mut self) -> bool {
        let t = self.clock();
        for i in 0..self.conns.len() {
            if (self.conns[i].needs_batch_flush(t) || self.conns[i].has_queued_packets())
                && !self.flush_link(i)
            {
                return false;
            }
        }
        true
    }

    /// Mirror of `handle_housekeeping` (minus registration I/O) followed by
    /// `SharedStats::update_with_link_cc`.
    fn housekeeping(&mut self) {
        let t = self.clock();
        let classic = self.snapshot.mode.is_classic();
        for i in 0..self.conns.len() {
            if self.conns[i].is_timed_out(t) {
                if self.conns[i].should_attempt_reconnect(t) {
                    self.conns[i].record_reconnect_attempt(t);
                    recover_connection(&mut self.conns[i], &mut self.seq_tracker);
                    self.pending_wire.remove(&i);
                    self.links[i].awaiting_reg3 = true;
                    self.links[i].host.push_back(WirePacket {
                        up: Up::Reg2,
                        bytes: 258,
                        enq_ms: self.now,
                    });
                    self.links[i].host_bytes += 258.0;
                }
                continue;
            }
            if self.conns[i].needs_keepalive(t) {
                let ka = self.conns[i].keepalive_packet(t).to_vec();
                self.links[i].host_bytes += ka.len() as f64;
                let len = ka.len();
                self.links[i].host.push_back(WirePacket {
                    up: Up::Keepalive(ka),
                    bytes: len,
                    enq_ms: self.now,
                });
            }
            if self.conns[i].needs_rtt_measurement(t) {
                let ka = self.conns[i].keepalive_packet(t).to_vec();
                self.links[i].host_bytes += ka.len() as f64;
                let len = ka.len();
                self.links[i].host.push_back(WirePacket {
                    up: Up::Keepalive(ka),
                    bytes: len,
                    enq_ms: self.now,
                });
            }
            let c = &mut self.conns[i];
            if !classic {
                c.perform_window_recovery(t);
            }
            c.calculate_bitrate(t);
            c.update_phase(t);
            c.recompute_batch_regime();
        }
        let snapshots = self.link_cc.tick_all(&self.conns, t);
        for c in self.conns.iter_mut() {
            let snap = snapshots.get(&c.conn_id);
            c.cc_backing_off = snap.is_some_and(|s| s.state == CcState::BackingOff);
            c.cc_target_bps = snap.map_or(0, |s| s.target_bps);
            c.loss_degraded = snap.is_some_and(|s| s.loss_degraded);
        }
        // `SharedStats::update`'s per-link figures.
        self.published_links = self
            .conns
            .iter()
            .map(|c| {
                let active = c.connected && !c.is_timed_out(t);
                LinkCapacity {
                    enabled: c.admin_enabled,
                    payload_eligible: c.admin_enabled
                        && active
                        && c.is_schedulable()
                        && !c.stall_latched()
                        && !c.is_quality_excluded(),
                    capacity_ready: c.cc_target_bps > 0,
                    target_bps: c.cc_target_bps,
                    delivered_bps: c.current_delivered_bps(),
                }
            })
            .collect();
    }

    /// Mirror of `process_uplink_packet` + `process_connection_events` for
    /// everything that arrived back on link `idx` this step.
    fn sender_receive(&mut self, idx: usize, arrivals: Vec<Down>) {
        let t = self.clock();
        let classic = self.snapshot.mode.is_classic();
        let mut acks = Vec::new();
        let mut naks = Vec::new();
        let mut srtla_acks = Vec::new();
        let mut srt_acks_full = Vec::new();
        for down in arrivals {
            self.conns[idx].last_received = Some(t);
            match down {
                Down::SrtAck {
                    ack,
                    rtt_ms,
                    sent_ms,
                } => {
                    acks.push(ack);
                    srt_acks_full.push((ack, rtt_ms, sent_ms));
                }
                Down::SrtNak(list) => naks.extend(list),
                Down::SrtlaAck(list) => srtla_acks.extend(list),
                Down::KeepaliveEcho(data) => {
                    let c = &mut self.conns[idx];
                    let label = c.label.clone();
                    if c.rtt.handle_keepalive_response(&data, &label, t).is_some() {
                        c.record_rtt_probe();
                        c.last_ack_or_rtt_sample_ms = t;
                    }
                }
                Down::Reg3 => {
                    let c = &mut self.conns[idx];
                    c.clear_pre_registration_state(t);
                    c.connected = true;
                    c.last_received = Some(t);
                    let label = c.label.clone();
                    c.reconnection.mark_success(&label);
                    self.links[idx].awaiting_reg3 = false;
                }
            }
        }
        // process_connection_events
        for ack in &acks {
            let owner = self.seq_tracker.get(*ack, t);
            for c in self.conns.iter_mut() {
                let owns = owner == Some(c.conn_id);
                c.handle_srt_ack(*ack as i32, t, owns);
            }
        }
        for srtla_ack in &srtla_acks {
            let conn_id = self.conns[idx].conn_id;
            let delivery = self
                .seq_tracker
                .acknowledge_delivery(*srtla_ack, conn_id, t);
            if let Some(credit) = delivery
                && credit.newly_credited
            {
                self.conns[idx].record_delivered_bytes(u64::from(credit.wire_bytes));
            }
            let found = self.conns[idx].handle_srtla_ack_specific(*srtla_ack as i32, classic, t);
            if !found
                && let Some(credit) = delivery
                && credit.newly_credited
            {
                self.conns[idx].record_sweep_pruned_delivery_proof(credit.timestamp_ms, t);
            }
            if !(found || delivery.is_some()) {
                for (i, c) in self.conns.iter_mut().enumerate() {
                    if i != idx && c.handle_srtla_ack_specific(*srtla_ack as i32, classic, t) {
                        break;
                    }
                }
            }
            for c in self.conns.iter_mut() {
                c.handle_srtla_ack_global();
            }
        }
        for nak in &naks {
            // attribute_nak
            let mut counted = false;
            if let Some(conn_id) = self.seq_tracker.get(*nak, t)
                && let Some(pos) = self.conns.iter().position(|c| c.conn_id == conn_id)
            {
                self.conns[pos].handle_nak(*nak as i32, t);
                counted = true;
            }
            if !counted {
                for c in self.conns.iter_mut() {
                    if c.handle_nak(*nak as i32, t) {
                        break;
                    }
                }
            }
        }
        // Forward to the SRT sender.
        for (ack, rtt, sent) in srt_acks_full {
            self.srt_on_ack(ack, rtt, sent);
        }
        if !naks.is_empty() {
            self.srt_on_nak(&naks);
        }
    }

    // ------------------------------------------------------------- network

    fn advance_links(&mut self) {
        let dt = STEP_MS as f64 / 1_000.0;
        let now = self.now;
        for link in &mut self.links {
            let capacity = link.cfg.capacity_at(now) as f64;
            let base = link.cfg.base_rtt_at(now);
            // Host -> modem (back-pressure when the modem is full).
            while let Some(front) = link.host.front() {
                if link.modem_bytes + front.bytes as f64 > link.cfg.modem_buffer_bytes {
                    break;
                }
                let w = link.host.pop_front().expect("front");
                link.host_bytes -= w.bytes as f64;
                link.modem_bytes += w.bytes as f64;
                link.modem.push_back(w);
            }
            // Modem sends what its grant allows.
            link.credit += capacity / 8.0 * dt;
            if link.modem.is_empty() {
                link.credit = link.credit.min(SRT_DATA_BYTES as f64);
            }
            while let Some(front) = link.modem.front() {
                if (front.bytes as f64) > link.credit {
                    break;
                }
                let w = link.modem.pop_front().expect("front");
                link.credit -= w.bytes as f64;
                link.modem_bytes -= w.bytes as f64;
                link.carried_bytes += w.bytes as f64;
                let noise = link.rng.unit() * link.cfg.rtt_noise_ms / 2.0;
                let arrival = now + (base / 2.0 + noise) as u64;
                link.up_flight.push_back((arrival, w));
            }
            let queue_ms = link.queue_delay_ms(now);
            link.max_queue_ms = link.max_queue_ms.max(queue_ms);
        }
    }

    fn send_down(&mut self, idx: usize, down: Down) {
        let now = self.now;
        let link = &mut self.links[idx];
        let base = link.cfg.base_rtt_at(now);
        let noise = link.rng.unit() * link.cfg.rtt_noise_ms / 2.0;
        let arrival = now + (base / 2.0 + noise).max(1.0) as u64;
        link.down_flight.push_back((arrival, down));
    }

    /// srtla_rec + SRT receiver.
    fn receiver_step(&mut self) {
        let now = self.now;
        let latency = u64::from(self.cfg.latency_ms);
        for idx in 0..self.links.len() {
            let mut arrived = Vec::new();
            while self.links[idx]
                .up_flight
                .front()
                .is_some_and(|(at, _)| *at <= now)
            {
                arrived.push(self.links[idx].up_flight.pop_front().expect("front").1);
            }
            for w in arrived {
                match w.up {
                    Up::Data {
                        seq,
                        media_ms,
                        sent_ms,
                        retransmit,
                    } => {
                        self.rx.last_active_link = idx;
                        if self.links[idx].srtla_ack_pending.is_empty() {
                            self.links[idx].srtla_ack_first_ms = now;
                        }
                        self.links[idx].srtla_ack_pending.push(seq);
                        if self.links[idx].srtla_ack_pending.len() >= SRTLA_ACK_EVERY {
                            let acks = std::mem::take(&mut self.links[idx].srtla_ack_pending);
                            self.send_down(idx, Down::SrtlaAck(acks));
                        }
                        if !retransmit {
                            self.rx.transit.push_back((now, (now - sent_ms) as f64));
                        }
                        self.receive_data(seq, media_ms);
                    }
                    Up::AckAck { ack_sent_ms } => {
                        let sample = (now - ack_sent_ms) as f64;
                        self.rx.srtt += (sample - self.rx.srtt) / 8.0;
                    }
                    Up::Keepalive(data) => self.send_down(idx, Down::KeepaliveEcho(data)),
                    Up::Reg2 => self.send_down(idx, Down::Reg3),
                }
            }
        }
        if let Some(flush_ms) = self.cfg.srtla_ack_flush_ms {
            for idx in 0..self.links.len() {
                let link = &self.links[idx];
                if !link.srtla_ack_pending.is_empty() && now >= link.srtla_ack_first_ms + flush_ms {
                    let acks = std::mem::take(&mut self.links[idx].srtla_ack_pending);
                    self.send_down(idx, Down::SrtlaAck(acks));
                }
            }
        }
        while self
            .rx
            .transit
            .front()
            .is_some_and(|(at, _)| *at + 1_000 < now)
        {
            self.rx.transit.pop_front();
        }
        // Receiver gives up on data whose play time has passed.
        let expired: Vec<u32> = self
            .rx
            .missing
            .keys()
            .copied()
            .filter(|s| self.media_of.get(s).is_some_and(|m| now > m + latency))
            .collect();
        for s in expired {
            self.rx.missing.remove(&s);
            if let Some(m) = self.media_of.get(&s) {
                self.bad_slices.insert(m / 100);
            }
            self.late_or_lost_packets += 1;
            self.rx.received.insert(s);
        }
        while self.rx.received.remove(&self.rx.next_needed) {
            self.rx.next_needed += 1;
        }
        // Periodic cumulative ACK over the last active link.
        if now >= self.rx.last_ack_ms + SRT_ACK_INTERVAL_MS && self.rx.max_seen.is_some() {
            self.rx.last_ack_ms = now;
            let link = self.rx.last_active_link;
            let down = Down::SrtAck {
                ack: self.rx.next_needed,
                rtt_ms: self.rx.srtt,
                sent_ms: now,
            };
            self.send_down(link, down);
        }
        // NAKs after the reorder hold (cross-link transit spread).
        let spread = {
            let mut lo = f64::INFINITY;
            let mut hi = 0.0_f64;
            for (_, tr) in &self.rx.transit {
                lo = lo.min(*tr);
                hi = hi.max(*tr);
            }
            if lo.is_finite() { hi - lo } else { 0.0 }
        };
        let hold = spread.min(latency as f64 / 4.0) as u64;
        let renak = (self.rx.srtt + 20.0) as u64;
        let mut nak = Vec::new();
        for (seq, (detected, last)) in self.rx.missing.iter_mut() {
            if now < *detected + hold {
                continue;
            }
            if last.is_some_and(|l| now < l + renak) {
                continue;
            }
            *last = Some(now);
            nak.push(*seq);
            if nak.len() >= 64 {
                break;
            }
        }
        if !nak.is_empty() {
            let link = self.rx.last_active_link;
            self.send_down(link, Down::SrtNak(nak));
        }
    }

    fn receive_data(&mut self, seq: u32, media_ms: u64) {
        let now = self.now;
        if seq < self.rx.next_needed || self.rx.received.contains(&seq) {
            return;
        }
        self.rx.missing.remove(&seq);
        let latency = u64::from(self.cfg.latency_ms);
        if now.saturating_sub(media_ms) <= latency {
            self.on_time_packets += 1;
        } else {
            self.late_or_lost_packets += 1;
            self.bad_slices.insert(media_ms / 100);
        }
        self.rx.received.insert(seq);
        match self.rx.max_seen {
            Some(max) if seq > max + 1 => {
                for missing in (max + 1)..seq {
                    if !self.rx.received.contains(&missing) {
                        self.rx.missing.insert(missing, (now, None));
                    }
                }
                self.rx.max_seen = Some(seq);
            }
            Some(max) if seq <= max => {}
            _ => self.rx.max_seen = Some(seq),
        }
    }

    fn sender_step(&mut self) {
        let now = self.now;
        for idx in 0..self.links.len() {
            let mut arrivals = Vec::new();
            while self.links[idx]
                .down_flight
                .front()
                .is_some_and(|(at, _)| *at <= now)
            {
                arrivals.push(self.links[idx].down_flight.pop_front().expect("front").1);
            }
            if !arrivals.is_empty() && !self.links[idx].awaiting_reg3
                || arrivals.iter().any(|d| matches!(d, Down::Reg3))
            {
                self.sender_receive(idx, arrivals);
            }
        }
    }

    /// One 5 ms step of the SRTLA sender loop.
    fn srtla_step(&mut self) {
        // A blocked send completes once the host queue has room.
        if let Some((idx, wire)) = self.blocked.take()
            && !self.push_host(idx, wire)
        {
            self.blocked_ms += STEP_MS;
            return;
        }
        while let Some(packet) = self.srtla_input.pop_front() {
            if !self.srtla_forward(packet) {
                self.blocked_ms += STEP_MS;
                return;
            }
        }
        if self.now.is_multiple_of(BATCH_FLUSH_MS) && !self.flush_all() {
            self.blocked_ms += STEP_MS;
            return;
        }
        if self.now.is_multiple_of(HOUSEKEEPING_MS) && self.now > 0 {
            self.housekeeping();
        }
    }

    pub fn report(&self, from_ms: u64, to_ms: u64, trace: &[TracePoint]) -> BondReport {
        let slices = self
            .bad_slices
            .range(from_ms / 100..to_ms.div_ceil(100))
            .count() as u64;
        let total: f64 = self.links.iter().map(|l| l.carried_bytes).sum();
        let window: Vec<_> = trace.iter().filter(|p| p.t_ms >= from_ms).collect();
        BondReport {
            glitch_ms: slices * 100,
            on_time_fraction: self.on_time_packets as f64
                / (self.on_time_packets + self.late_or_lost_packets).max(1) as f64,
            mean_video_bps: window.iter().map(|p| p.encoder_bps).sum::<f64>()
                / window.len().max(1) as f64,
            link_share: self
                .links
                .iter()
                .map(|l| l.carried_bytes / total.max(1.0))
                .collect(),
            retransmit_fraction: self.retransmitted as f64 / self.produced_packets.max(1) as f64,
            blocked_ms: self.blocked_ms,
            max_link_queue_ms: self.links.iter().map(|l| l.max_queue_ms).collect(),
            gate_events: self.conns.iter().map(|c| c.stall_gate_events()).collect(),
        }
    }
}

/// Run the full system (encoder, ABR, shedding, SRT, SRTLA, links).
pub fn run_bond(cfg: BondConfig, duration_ms: u64) -> (BondSim, Vec<TracePoint>) {
    let sim_cfg = cfg.abr_config();
    let start = 3_000_000;
    let mut abr: AbrController = controller_for(&sim_cfg, start);
    let mut sim = BondSim::new(cfg.clone());
    let mut target = start;
    let mut encoder = start as f64;
    let mut shedder = VideoShedder::default();
    let mut shed_estimator = Estimator::default();
    let mut last_stats: Option<SrtCapacity> = None;
    let mut video_admitted = true;
    let mut noise = Rng::new(99);
    let mut trace = Vec::new();
    while sim.now < duration_ms {
        sim.now += STEP_MS;
        let now = sim.now;
        if now.is_multiple_of(ABR_TICK_MS) {
            let alpha = (ABR_TICK_MS as f64 / cfg.encoder_tau_ms).min(1.0);
            encoder += (target as f64 - encoder) * alpha;
            if cfg.shed {
                let keyframe = now.is_multiple_of(KEYFRAME_INTERVAL_MS);
                let path = last_stats.as_ref().map(|srt| {
                    let floor = shed_estimator.rtt_floor_ms();
                    PathState {
                        queue_ms: f64::from(srt.send_buffer_ms),
                        rtt_floor_ms: floor,
                        latency_ms: f64::from(srt.latency_ms),
                        delivery_bps: shed_estimator
                            .delivery(500, floor as u64)
                            .map(|d| d.goodput_bps),
                    }
                });
                let bytes = (encoder / 8.0 * ABR_TICK_MS as f64 / 1_000.0) as usize;
                let priority = if keyframe { PRIORITY_HIGHEST } else { 2 };
                video_admitted = shedder.admit(path.as_ref(), bytes, keyframe, priority);
            }
        }
        let jitter = 1.0 + (noise.unit() * 2.0 - 1.0) * cfg.encoder_noise;
        let video = if video_admitted {
            encoder * jitter
        } else {
            0.0
        };
        let bytes = (video + cfg.audio_bps as f64) / 8.0 * STEP_MS as f64 / 1_000.0;
        sim.srt_send_media(bytes);
        sim.srtla_step();
        sim.advance_links();
        sim.receiver_step();
        sim.sender_step();
        sim.srt_sender_drop_old();
        if now.is_multiple_of(ABR_TICK_MS) {
            let stats = sim.srt_stats();
            shed_estimator.push(now, &stats);
            last_stats = Some(stats.clone());
            let debug_stats = stats.clone();
            let mut sample: AbrSample = sample_for(&sim_cfg, now, target, stats);
            if !sim.published_links.is_empty() {
                sample.links = sim.published_links.clone();
            }
            let decision = abr.decide(&sample);
            target = decision.applied_bps;
            if std::env::var("BOND_LINKS").is_ok() && now.is_multiple_of(500) {
                let t = sim.clock();
                let mut line = format!("{now:>6} tgt {:>5}", target / 1000);
                for (i, c) in sim.conns.iter().enumerate() {
                    let l = &sim.links[i];
                    line += &format!(
                        " | {} cap {:>5} win {:>5} inf {:>3} cc {:>5} q {:>4.0}ms {:?}{}{}",
                        l.cfg.name,
                        l.cfg.capacity_at(now) / 1000,
                        c.window,
                        c.in_flight_packets,
                        c.cc_target_bps / 1000,
                        l.queue_delay_ms(now),
                        c.phase,
                        if c.is_stall_gated() { " GATED" } else { "" },
                        if c.is_quality_excluded() {
                            " QEXCL"
                        } else {
                            ""
                        },
                    );
                    let _ = t;
                }
                eprintln!("{line}");
            }
            if std::env::var("BOND_DEBUG").is_ok() && now.is_multiple_of(200) {
                eprintln!(
                    "{now:>6} tgt {:>6} rtt {:>4} sndbuf {:>5} flight {:>4} sent {:>6} naks {:>3} gp {:>6} floor {:>4} {} | {}",
                    target / 1000,
                    debug_stats.rtt_ms,
                    debug_stats.send_buffer_ms,
                    debug_stats.packets_in_flight,
                    debug_stats.sent_unique_bytes,
                    debug_stats.sender_loss_packets,
                    decision.srt_capacity_bps / 1000,
                    decision.rtt_floor_ms,
                    decision.control_state,
                    abr.debug_state()
                );
            }
            let capacity: u64 = sim.links.iter().map(|l| l.cfg.capacity_at(now)).sum();
            trace.push(TracePoint {
                t_ms: now,
                target_bps: target,
                encoder_bps: if video_admitted { encoder } else { 0.0 },
                capacity_bps: capacity,
                rtt_ms: sim.sender_srtt,
                queue_ms: 0.0,
                lost_bytes: 0.0,
                one_way_ms: 0.0,
            });
        }
    }
    (sim, trace)
}

fn mobile(name: &'static str, seed: u64, mean_bps: f64, duration_ms: u64) -> LinkConfig {
    let (capacity, base_rtt) = crate::abr::sim::mobile_link(seed, mean_bps, duration_ms);
    LinkConfig {
        name,
        capacity,
        base_rtt,
        rtt_noise_ms: 15.0,
        ..LinkConfig::steady(name, 0, 60.0)
    }
}

/// Bonded scenarios: (name, links, duration).
pub fn bond_scenarios() -> Vec<(&'static str, Vec<LinkConfig>, u64)> {
    let mut out = Vec::new();
    out.push((
        "3 steady links 4/3/3",
        vec![
            LinkConfig::steady("a", 4_000_000, 40.0),
            LinkConfig::steady("b", 3_000_000, 60.0),
            LinkConfig::steady("c", 3_000_000, 80.0),
        ],
        90_000,
    ));
    out.push((
        "3 moving modems",
        vec![
            mobile("a", 31, 4e6, 180_000),
            mobile("b", 32, 3e6, 180_000),
            mobile("c", 33, 3e6, 180_000),
        ],
        180_000,
    ));
    let mut tunnel = LinkConfig::steady("c", 3_000_000, 70.0);
    tunnel.capacity = vec![(0, 3_000_000), (60_000, 30_000), (68_000, 3_000_000)];
    out.push((
        "one modem in an 8 s tunnel",
        vec![
            LinkConfig::steady("a", 4_000_000, 45.0),
            LinkConfig::steady("b", 3_000_000, 55.0),
            tunnel,
        ],
        120_000,
    ));
    let (rat_cfg, phases) = crate::abr::sim::rat_switching(5, &crate::abr::sim::rat_sequence());
    let rat_duration = phases.last().map_or(0, |p| p.end_ms);
    out.push((
        "one modem 5G/LTE/HSPA+, two LTE",
        vec![
            LinkConfig {
                name: "a",
                capacity: rat_cfg.capacity.clone(),
                base_rtt: rat_cfg.base_rtt.clone(),
                rtt_noise_ms: 10.0,
                ..LinkConfig::steady("a", 0, 30.0)
            },
            mobile("b", 41, 5e6, rat_duration),
            mobile("c", 42, 5e6, rat_duration),
        ],
        rat_duration,
    ));
    let (leo_capacity, leo_rtt) = crate::abr::sim::leo(11, 180_000);
    out.push((
        "Starlink + 2 modems",
        vec![
            LinkConfig {
                name: "sat",
                capacity: leo_capacity,
                base_rtt: leo_rtt,
                rtt_noise_ms: 10.0,
                ..LinkConfig::steady("sat", 0, 45.0)
            },
            mobile("b", 51, 4e6, 180_000),
            mobile("c", 52, 3e6, 180_000),
        ],
        180_000,
    ));
    let mut a = LinkConfig::steady("a", 4_000_000, 50.0);
    a.capacity = vec![(0, 4_000_000), (30_000, 1_500_000), (45_000, 4_000_000)];
    let mut b = LinkConfig::steady("b", 4_000_000, 60.0);
    b.capacity = vec![(0, 4_000_000), (30_000, 1_500_000), (45_000, 4_000_000)];
    out.push(("2 modems, same congested cell", vec![a, b], 90_000));
    out
}

pub const BOND_HOST_QUEUES: [(&str, f64); 3] = [
    ("1.3 MB", 1_000.0 * 1_316.0),
    ("256 KB", 256.0 * 1024.0),
    ("120 KB", 120.0 * 1024.0),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn print(name: &str, r: &BondReport) {
        eprintln!(
            "{name:<40} glitch {:>5} ms  on-time {:>6.2}%  video {:>5.2} Mb  retx {:>5.2}%  blocked {:>5} ms  share {:?}  max queue {:?}  gates {:?}",
            r.glitch_ms,
            r.on_time_fraction * 100.0,
            r.mean_video_bps / 1e6,
            r.retransmit_fraction * 100.0,
            r.blocked_ms,
            r.link_share
                .iter()
                .map(|s| format!("{:.2}", s))
                .collect::<Vec<_>>(),
            r.max_link_queue_ms
                .iter()
                .map(|q| format!("{:.0}", q))
                .collect::<Vec<_>>(),
            r.gate_events
        );
    }

    /// Guards for the bonded system (real SRTLA scheduler). Bounds sit
    /// below measured values with margin. Throughput on moving links is
    /// deliberately not asserted: it is limited by the vendored scheduler's
    /// per-link congestion control (see the README), not by this plugin.
    #[test]
    fn bonded_links_meet_targets() {
        let scenarios = bond_scenarios();
        for (name, links, duration) in scenarios {
            let cfg = BondConfig::new(links).with_host_queue(120.0 * 1024.0);
            let (sim, trace) = run_bond(cfg, duration);
            let r = sim.report(10_000, duration - 3_000, &trace);
            print(name, &r);
            assert_eq!(r.glitch_ms, 0, "{name}: late or lost media");
            assert_eq!(r.blocked_ms, 0, "{name}: sender blocked on a full socket");
            let floor = match name {
                "3 steady links 4/3/3" => 5_500_000.0,
                "one modem in an 8 s tunnel" => 5_000_000.0,
                "2 modems, same congested cell" => 3_800_000.0,
                _ => 0.0,
            };
            assert!(
                r.mean_video_bps >= floor,
                "{name}: video {:.0}",
                r.mean_video_bps
            );
        }
        // A receiver that also flushes SRTLA ACKs on a timer lets the
        // scheduler use the whole bond.
        let (_, links, duration) = bond_scenarios().remove(0);
        let mut cfg = BondConfig::new(links);
        cfg.srtla_ack_flush_ms = Some(10);
        let (sim, trace) = run_bond(cfg, duration);
        let r = sim.report(10_000, duration - 3_000, &trace);
        assert!(
            r.mean_video_bps >= 7_800_000.0,
            "flush receiver: {:.0}",
            r.mean_video_bps
        );
    }

    #[test]
    #[ignore]
    fn bond_smoke() {
        let cfg = BondConfig::new(vec![
            LinkConfig::steady("a", 4_000_000, 40.0),
            LinkConfig::steady("b", 3_000_000, 60.0),
            LinkConfig::steady("c", 3_000_000, 80.0),
        ]);
        let (sim, trace) = run_bond(cfg.clone(), 60_000);
        print("3 steady links 4/3/3", &sim.report(10_000, 57_000, &trace));
        let mut timed = cfg;
        timed.srtla_ack_flush_ms = Some(10);
        let (sim2, trace2) = run_bond(timed, 60_000);
        print(
            "  same, receiver ACK flush 10 ms",
            &sim2.report(10_000, 57_000, &trace2),
        );
        for p in trace.iter().filter(|p| p.t_ms % 5_000 == 0) {
            eprintln!(
                "  {:>6} tgt {:>6} kb/s enc {:>6.0} rtt {:>5.0}",
                p.t_ms,
                p.target_bps / 1000,
                p.encoder_bps / 1000.0,
                p.rtt_ms
            );
        }
    }
}

#[cfg(test)]
mod reports {
    use super::*;

    #[test]
    #[ignore]
    fn bond_report() {
        for (receiver, flush) in [
            ("classic srtla_rec (ACK every 10 pkts)", None),
            ("receiver ACK flush 10 ms", Some(10)),
        ] {
            eprintln!("== {receiver}");
            eprintln!(
                "{:<34} {:<7} {:>8} {:>8} {:>8} {:>6} {:>8}  share % / max link queue ms",
                "scenario", "host q", "glitch", "on-time", "video", "retx", "blocked"
            );
            for (name, links, duration) in bond_scenarios() {
                for (qname, q) in BOND_HOST_QUEUES {
                    let mut cfg = BondConfig::new(links.clone()).with_host_queue(q);
                    cfg.srtla_ack_flush_ms = flush;
                    let (sim, trace) = run_bond(cfg, duration);
                    let r = sim.report(10_000, duration - 3_000, &trace);
                    eprintln!(
                        "{name:<34} {qname:<7} {:>5} ms {:>7.2}% {:>5.2} Mb {:>5.2}% {:>5} ms  {:?} / {:?}",
                        r.glitch_ms,
                        r.on_time_fraction * 100.0,
                        r.mean_video_bps / 1e6,
                        r.retransmit_fraction * 100.0,
                        r.blocked_ms,
                        r.link_share
                            .iter()
                            .map(|s| (s * 100.0).round() as u32)
                            .collect::<Vec<_>>(),
                        r.max_link_queue_ms
                            .iter()
                            .map(|q| q.round() as u32)
                            .collect::<Vec<_>>(),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod debug {
    use super::*;

    #[test]
    #[ignore]
    fn bond_scenario_trace() {
        let which: usize = std::env::var("SCENARIO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let to: u64 = std::env::var("TO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60_000);
        let (name, links, _) = bond_scenarios().remove(which);
        eprintln!("{name}");
        let mut cfg = BondConfig::new(links);
        if std::env::var("FLUSH").is_ok() {
            cfg.srtla_ack_flush_ms = Some(10);
        }
        let _ = run_bond(cfg, to);
    }
}
