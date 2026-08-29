#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod abr;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use srtla_send::embedded::{
    EmbeddedLink, EmbeddedSender, EmbeddedSenderConfig, EmbeddedSubmitError,
};

pub use abr::{AbrConfig, AbrController, AbrDecision, AbrSample, LinkCapacity, SrtCapacity};

const ENGINE_INPUT_CAPACITY: usize = 1024;
const ENGINE_OUTPUT_CAPACITY: usize = 256;

const SESSION_IDLE: u32 = 0;
const SESSION_CONNECTING: u32 = 1;
const SESSION_CONNECTED: u32 = 2;
const SESSION_RECONNECTING: u32 = 3;
const SESSION_FATAL: u32 = 4;
const SESSION_STOPPED: u32 = 5;
const SRT_STATS_MAX_AGE_MS: u64 = 200;

thread_local! {
    // The ABI returns a pointer. Keep the pointed-to bytes in caller-thread
    // storage instead of exposing memory
    // owned by the engine mutex (which another thread can mutate or free).
    static LAST_ERROR_VIEW: RefCell<CString> = RefCell::new(CString::new("").expect("empty CString"));
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct EngineConfig {
    #[serde(default)]
    receiver_host: String,
    #[serde(default)]
    receiver_port: u16,
    #[serde(default)]
    stream_id: String,
    #[serde(default)]
    latency_ms: u32,
    #[serde(default)]
    passphrase: String,
    #[serde(default)]
    pbkeylen: u32,
    #[serde(default)]
    scheduler: String,
    #[serde(default)]
    links: Vec<EngineLinkConfig>,
    #[serde(default)]
    abr: Option<AbrConfig>,
    #[serde(default)]
    audio_bps: u64,
}

#[derive(Debug, Deserialize)]
struct EngineLinkConfig {
    id: u64,
    label: String,
    #[serde(default)]
    address: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    capacity_ready: bool,
    #[serde(default)]
    payload_eligible: bool,
    #[serde(default)]
    target_bps: u64,
    #[serde(default)]
    quality_percent: u8,
    #[serde(default)]
    rtt_ms: u32,
    #[serde(default)]
    loss_permille: u32,
    #[serde(default)]
    nak_count: u64,
    #[serde(default)]
    used_bps: u64,
    #[serde(default)]
    delivered_bps: u64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    cc_state: String,
    #[serde(default)]
    stall_gate_events: u64,
    #[serde(default)]
    exclusion_reason: String,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize)]
struct LinkSnapshot {
    id: u64,
    label: String,
    admin_enabled: bool,
    connected: bool,
    payload_eligible: bool,
    capacity_ready: bool,
    target_bps: u64,
    state: String,
    quality_percent: u8,
    rtt_ms: u32,
    loss_permille: u32,
    nak_count: u64,
    used_bps: u64,
    delivered_bps: u64,
    cc_state: String,
    stall_gate_events: u64,
    exclusion_reason: String,
}

#[derive(Clone, Debug, Serialize)]
struct EngineSnapshot {
    receiver_host: String,
    receiver_port: u16,
    running: bool,
    state: String,
    error: String,
    queue_in: usize,
    queue_out: usize,
    link_capacity_bps: u64,
    srt_bandwidth_bps: u64,
    srt_capacity_bps: u64,
    srt_send_rate_bps: u64,
    srt_send_buffer_ms: u32,
    srt_send_buffer_packets: u32,
    srt_packets_in_flight: u32,
    srt_rtt_ms: u32,
    srt_latency_ms: u32,
    srt_sender_loss_packets: u32,
    srt_retransmit_permille: u32,
    srt_dropped_bytes: u64,
    srt_stats_ready: bool,
    srt_limited: bool,
    transport_stressed: bool,
    estimated_capacity_bps: u64,
    recommended_video_bps: u64,
    current_video_bps: u64,
    abr_state: String,
    abr_queue_light_packets: u32,
    abr_queue_heavy_packets: u32,
    abr_queue_severe_packets: u32,
    abr_rtt_increase_below_ms: u32,
    abr_rtt_decrease_above_ms: u32,
    srt_session_state: String,
    srt_connected: bool,
    links: Vec<LinkSnapshot>,
}

#[derive(Debug, Deserialize)]
struct RunnerStats {
    #[serde(default)]
    links: Vec<RunnerLinkStats>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: String,
}

#[derive(Debug, Deserialize)]
struct RunnerLinkStats {
    ip: std::net::IpAddr,
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    timed_out: bool,
    #[serde(default)]
    admin_enabled: bool,
    #[serde(default)]
    payload_eligible: bool,
    #[serde(default)]
    capacity_ready: bool,
    #[serde(default)]
    cc_target_bps: u64,
    #[serde(default)]
    quality_multiplier: f64,
    #[serde(default)]
    rtt_ms: u32,
    #[serde(default)]
    bitrate_bytes_per_sec: u32,
    #[serde(default)]
    delivered_bps: u64,
    #[serde(default)]
    cc_state: String,
    #[serde(default)]
    stall_gated: bool,
    #[serde(default)]
    stall_gate_events: u64,
    #[serde(default)]
    weak_reason: String,
    #[serde(default)]
    cc_loss_degraded: bool,
    #[serde(default)]
    cc_loss_permille: u32,
    #[serde(default)]
    nak_count: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SrtlaSrtStats {
    pub struct_size: u32,
    pub reserved: u32,
    pub sampled_at_ms: u64,
    pub bandwidth_bps: u64,
    pub send_rate_bps: u64,
    pub sent_unique_bytes: u64,
    pub retransmitted_bytes: u64,
    pub dropped_bytes: u64,
    pub send_buffer_ms: u32,
    pub packets_in_flight: u32,
    pub sender_loss_packets: u32,
    pub reserved2: u32,
    pub rtt_ms: u32,
    pub send_buffer_packets: u32,
    pub latency_ms: u32,
    pub reserved3: u32,
}

#[derive(Clone, Debug, Default)]
struct SrtTransportStats {
    sampled_at_ms: u64,
    bandwidth_bps: u64,
    send_rate_bps: u64,
    sent_unique_bytes: u64,
    retransmitted_bytes: u64,
    dropped_bytes: u64,
    send_buffer_ms: u32,
    send_buffer_packets: u32,
    packets_in_flight: u32,
    rtt_ms: u32,
    latency_ms: u32,
    sender_loss_packets: u32,
}

impl SrtTransportStats {
    fn retransmit_permille(&self) -> u32 {
        if self.retransmitted_bytes == 0 {
            return 0;
        }
        let ratio = u128::from(self.retransmitted_bytes) * 1_000
            / u128::from(self.sent_unique_bytes.max(1));
        ratio.min(1_000) as u32
    }
}

struct DatagramQueue {
    queue: Mutex<VecDeque<Vec<u8>>>,
    changed: Condvar,
    capacity: usize,
    closed: AtomicBool,
}

impl DatagramQueue {
    fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            changed: Condvar::new(),
            capacity,
            closed: AtomicBool::new(false),
        }
    }

    fn push(&self, packet: Vec<u8>) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.len() >= self.capacity {
            return false;
        }
        queue.push_back(packet);
        self.changed.notify_one();
        true
    }

    fn pop(&self, timeout_ms: u32) -> Option<Vec<u8>> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.is_empty() && timeout_ms > 0 {
            let (guard, _) = self
                .changed
                .wait_timeout(queue, std::time::Duration::from_millis(timeout_ms as u64))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue = guard;
        }
        if queue.is_empty() && self.closed.load(Ordering::Acquire) {
            return None;
        }
        queue.pop_front()
    }

    fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    fn front_len(&self) -> Option<usize> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .front()
            .map(Vec::len)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    fn reopen(&self) {
        self.closed.store(false, Ordering::Release);
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

struct Engine {
    config: EngineConfig,
    input: DatagramQueue,
    output: DatagramQueue,
    running: bool,
    abr: AbrController,
    current_video_bps: u64,
    audio_bps: u64,
    last_error: CString,
    session_state: u32,
    session_error: String,
    srt_stats: SrtTransportStats,
    last_abr_decision: AbrDecision,
    runner: Option<EmbeddedSender>,
}

impl Engine {
    fn start_runner(&mut self) {
        let links = self
            .config
            .links
            .iter()
            .filter_map(|link| {
                link.address.parse().ok().map(|address| EmbeddedLink {
                    id: link.id,
                    label: link.label.clone(),
                    address,
                    enabled: link.enabled,
                })
            })
            .collect();
        self.runner = Some(EmbeddedSender::start(EmbeddedSenderConfig {
            receiver_host: self.config.receiver_host.clone(),
            receiver_port: self.config.receiver_port,
            links,
            scheduler_classic: self.config.scheduler.eq_ignore_ascii_case("classic"),
        }));
    }

    fn sync_runner_stats(&mut self) {
        let Some(runner) = self.runner.as_ref() else {
            return;
        };
        let Ok(stats) = serde_json::from_str::<RunnerStats>(&runner.stats_json()) else {
            return;
        };
        if stats.state.eq_ignore_ascii_case("error") {
            self.last_error = CString::new(stats.error).unwrap_or_else(|_| {
                CString::new("embedded runner error").expect("literal CString")
            });
        }
        for update in stats.links {
            if let Some(link) = self
                .config
                .links
                .iter_mut()
                .find(|link| link.address == update.ip.to_string())
            {
                link.connected = update.connected && !update.timed_out;
                link.capacity_ready = update.capacity_ready;
                link.target_bps = update.cc_target_bps;
                link.quality_percent = (update.quality_multiplier.clamp(0.0, 1.0) * 100.0) as u8;
                link.rtt_ms = update.rtt_ms;
                link.loss_permille = update.cc_loss_permille;
                link.nak_count = update.nak_count.max(0) as u64;
                link.used_bps = u64::from(update.bitrate_bytes_per_sec) * 8;
                link.delivered_bps = update.delivered_bps;
                link.state = if !link.enabled {
                    "Standby".to_string()
                } else if !link.connected {
                    "Offline".to_string()
                } else if update.stall_gated {
                    "Stalled".to_string()
                } else if !update.capacity_ready {
                    "Warming".to_string()
                } else if update.cc_state == "backing_off" || update.cc_loss_degraded {
                    "Degraded".to_string()
                } else {
                    "Live".to_string()
                };
                link.cc_state = update.cc_state.clone();
                link.stall_gate_events = update.stall_gate_events;
                link.exclusion_reason = if !update.payload_eligible {
                    if update.stall_gated {
                        "stalled".to_string()
                    } else if update.cc_loss_degraded {
                        "sustained loss".to_string()
                    } else if !update.weak_reason.is_empty() {
                        update.weak_reason.clone()
                    } else {
                        "scheduler".to_string()
                    }
                } else {
                    String::new()
                };
                if update.admin_enabled != link.enabled {
                    link.enabled = update.admin_enabled;
                }
                link.address = update.ip.to_string();
                link.payload_eligible = update.payload_eligible;
            }
        }
    }

    fn snapshot(&self) -> EngineSnapshot {
        let srt_connected = self.session_state == SESSION_CONNECTED;
        let srt_session_state = match self.session_state {
            SESSION_CONNECTING => "Connecting",
            SESSION_CONNECTED => "Connected",
            SESSION_RECONNECTING => "Reconnecting",
            SESSION_FATAL => "Fatal",
            SESSION_STOPPED => "Stopped",
            _ => "Idle",
        }
        .to_string();
        let has_live_link = self
            .config
            .links
            .iter()
            .any(|l| l.enabled && l.connected && l.payload_eligible);
        let link_capacity_bps = self
            .config
            .links
            .iter()
            .filter(|l| l.enabled && l.payload_eligible && l.capacity_ready)
            .map(|l| l.target_bps)
            .sum();
        let decision_current = self.last_abr_decision.recommended_bps > 0;
        let estimated_capacity_bps = if decision_current {
            self.last_abr_decision.estimated_capacity_bps
        } else {
            0
        };
        let recommended_video_bps = if decision_current {
            self.last_abr_decision.recommended_bps
        } else {
            let any_connected = self.config.links.iter().any(|l| l.enabled && l.connected);
            if !any_connected {
                self.abr.config().min_bps
            } else {
                self.current_video_bps
            }
        };
        let abr_state = if decision_current {
            self.last_abr_decision.control_state.clone()
        } else if !srt_connected {
            "Disconnected".to_string()
        } else {
            "Waiting for SRT feedback".to_string()
        };
        EngineSnapshot {
            receiver_host: self.config.receiver_host.clone(),
            receiver_port: self.config.receiver_port,
            running: self.running,
            state: if !self.last_error.to_string_lossy().is_empty()
                || self.session_state == SESSION_FATAL
            {
                "Error".to_string()
            } else if !self.running {
                "Idle".to_string()
            } else if self.session_state == SESSION_CONNECTING {
                "Starting".to_string()
            } else if self.session_state == SESSION_RECONNECTING
                || self.session_state == SESSION_STOPPED
            {
                "Reconnecting".to_string()
            } else if srt_connected && has_live_link {
                "Live".to_string()
            } else if self.config.links.iter().any(|l| l.enabled) {
                "Reconnecting".to_string()
            } else {
                "Waiting for network".to_string()
            },
            error: if !self.last_error.to_string_lossy().is_empty() {
                self.last_error.to_string_lossy().into_owned()
            } else {
                self.session_error.clone()
            },
            queue_in: self.input.len(),
            queue_out: self.output.len(),
            link_capacity_bps,
            srt_bandwidth_bps: self.srt_stats.bandwidth_bps,
            srt_capacity_bps: if decision_current {
                self.last_abr_decision.srt_capacity_bps
            } else {
                0
            },
            srt_send_rate_bps: self.srt_stats.send_rate_bps,
            srt_send_buffer_ms: self.srt_stats.send_buffer_ms,
            srt_send_buffer_packets: self.srt_stats.send_buffer_packets,
            srt_packets_in_flight: self.srt_stats.packets_in_flight,
            srt_rtt_ms: self.srt_stats.rtt_ms,
            srt_latency_ms: self.srt_stats.latency_ms,
            srt_sender_loss_packets: self.srt_stats.sender_loss_packets,
            srt_retransmit_permille: self.srt_stats.retransmit_permille(),
            srt_dropped_bytes: self.srt_stats.dropped_bytes,
            srt_stats_ready: srt_connected
                && self.srt_stats.sampled_at_ms > 0
                && self.srt_stats.rtt_ms > 0
                && self.srt_stats.latency_ms > 0,
            srt_limited: decision_current && self.last_abr_decision.srt_limited,
            transport_stressed: decision_current && self.last_abr_decision.transport_stressed,
            estimated_capacity_bps,
            recommended_video_bps,
            current_video_bps: self.current_video_bps,
            abr_state,
            abr_queue_light_packets: self.last_abr_decision.queue_light_packets,
            abr_queue_heavy_packets: self.last_abr_decision.queue_heavy_packets,
            abr_queue_severe_packets: self.last_abr_decision.queue_severe_packets,
            abr_rtt_increase_below_ms: self.last_abr_decision.rtt_increase_below_ms,
            abr_rtt_decrease_above_ms: self.last_abr_decision.rtt_decrease_above_ms,
            srt_session_state,
            srt_connected,
            links: self
                .config
                .links
                .iter()
                .map(|l| LinkSnapshot {
                    id: l.id,
                    label: l.label.clone(),
                    admin_enabled: l.enabled,
                    connected: l.connected,
                    payload_eligible: l.enabled && l.payload_eligible && l.connected,
                    capacity_ready: l.capacity_ready,
                    target_bps: l.target_bps,
                    state: if !l.enabled {
                        "Standby".to_string()
                    } else if l.state.is_empty() {
                        "Warming".to_string()
                    } else {
                        l.state.clone()
                    },
                    quality_percent: l.quality_percent,
                    rtt_ms: l.rtt_ms,
                    loss_permille: l.loss_permille,
                    nak_count: l.nak_count,
                    used_bps: l.used_bps,
                    delivered_bps: l.delivered_bps,
                    cc_state: l.cc_state.clone(),
                    stall_gate_events: l.stall_gate_events,
                    exclusion_reason: l.exclusion_reason.clone(),
                })
                .collect(),
        }
    }

    fn apply_abr(&mut self, now_ms: u64) -> AbrDecision {
        let links = self
            .config
            .links
            .iter()
            .map(|link| LinkCapacity {
                enabled: link.enabled,
                payload_eligible: link.payload_eligible,
                capacity_ready: link.capacity_ready,
                target_bps: link.target_bps,
                delivered_bps: link.delivered_bps,
            })
            .collect::<Vec<_>>();
        let capacity_ready = links
            .iter()
            .any(|link| link.enabled && link.payload_eligible && link.capacity_ready);
        let all_links_down = self.session_state != SESSION_CONNECTED
            || !self
                .config
                .links
                .iter()
                .any(|link| link.enabled && link.connected);
        let decision = self.abr.decide(&AbrSample {
            links,
            audio_bps: self.audio_bps,
            current_video_bps: self.current_video_bps,
            now_ms,
            capacity_ready,
            all_links_down,
            srt: SrtCapacity {
                ready: self.session_state == SESSION_CONNECTED
                    && now_ms.saturating_sub(self.srt_stats.sampled_at_ms) <= SRT_STATS_MAX_AGE_MS,
                sampled_at_ms: self.srt_stats.sampled_at_ms,
                bandwidth_bps: self.srt_stats.bandwidth_bps,
                send_rate_bps: self.srt_stats.send_rate_bps,
                send_buffer_ms: self.srt_stats.send_buffer_ms,
                send_buffer_packets: self.srt_stats.send_buffer_packets,
                rtt_ms: self.srt_stats.rtt_ms,
                latency_ms: if self.srt_stats.latency_ms > 0 {
                    self.srt_stats.latency_ms
                } else {
                    self.config.latency_ms.max(1)
                },
                retransmit_permille: self.srt_stats.retransmit_permille(),
                dropped_bytes: self.srt_stats.dropped_bytes,
            },
        });
        self.current_video_bps = decision.applied_bps;
        self.last_abr_decision = decision.clone();
        decision
    }
}

fn with_engine<'a>(handle: *mut SrtlaEngineHandle) -> Option<MutexGuard<'a, Engine>> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: handles are created by create and consumed only through this ABI.
    let engine = unsafe { (*handle).engine };
    if engine.is_null() {
        return None;
    }
    let mutex: &'a Mutex<Engine> = unsafe { &*engine };
    mutex.lock().ok()
}

#[repr(C)]
pub struct SrtlaEngineHandle {
    engine: *mut Mutex<Engine>,
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_create(config_json: *const c_char) -> *mut SrtlaEngineHandle {
    let parsed = if config_json.is_null() {
        EngineConfig {
            receiver_host: String::new(),
            receiver_port: 0,
            stream_id: String::new(),
            latency_ms: 0,
            passphrase: String::new(),
            pbkeylen: 0,
            scheduler: String::new(),
            links: Vec::new(),
            abr: None,
            audio_bps: 0,
        }
    } else {
        // SAFETY: caller promises a NUL-terminated UTF-8 string for the duration of the call.
        let text = unsafe { CStr::from_ptr(config_json) }.to_string_lossy();
        match serde_json::from_str::<EngineConfig>(&text) {
            Ok(config) => config,
            Err(_) => return ptr::null_mut(),
        }
    };
    let abr = AbrController::new(parsed.abr.clone().unwrap_or_default());
    let start_bps = abr.config().start_bps;
    let audio_bps = parsed.audio_bps;
    let engine = Box::new(Engine {
        config: parsed,
        input: DatagramQueue::new(ENGINE_INPUT_CAPACITY),
        output: DatagramQueue::new(ENGINE_OUTPUT_CAPACITY),
        running: false,
        abr,
        current_video_bps: start_bps,
        audio_bps,
        last_error: CString::new("").expect("empty CString"),
        session_state: SESSION_IDLE,
        session_error: String::new(),
        srt_stats: SrtTransportStats::default(),
        last_abr_decision: AbrDecision::default(),
        runner: None,
    });
    Box::into_raw(Box::new(SrtlaEngineHandle {
        engine: Box::into_raw(Box::new(Mutex::new(*engine))),
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_start(handle: *mut SrtlaEngineHandle) -> i32 {
    match with_engine(handle) {
        Some(mut engine) => {
            engine.input.reopen();
            engine.output.reopen();
            engine.last_error = CString::new("").expect("empty CString");
            engine.session_state = SESSION_IDLE;
            engine.session_error.clear();
            engine.srt_stats = SrtTransportStats::default();
            engine.last_abr_decision = AbrDecision::default();
            let current_video_bps = engine.current_video_bps;
            engine.abr.reset_for_bitrate(current_video_bps);
            if engine.runner.is_none()
                && engine
                    .config
                    .links
                    .iter()
                    .any(|link| link.address.parse::<std::net::IpAddr>().is_ok())
            {
                engine.start_runner();
            }
            engine.running = true;
            0
        }
        None => -1,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_stop(handle: *mut SrtlaEngineHandle) -> i32 {
    match with_engine(handle) {
        Some(mut engine) => {
            engine.running = false;
            engine.session_state = SESSION_STOPPED;
            engine.session_error.clear();
            engine.srt_stats = SrtTransportStats::default();
            engine.last_abr_decision = AbrDecision::default();
            engine.input.close();
            engine.output.close();
            if let Some(mut runner) = engine.runner.take() {
                runner.stop();
            }
            0
        }
        None => -1,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_destroy(handle: *mut SrtlaEngineHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: ownership is returned by create and destroy is terminal.
    unsafe {
        let handle = Box::from_raw(handle);
        if !handle.engine.is_null() {
            drop(Box::from_raw(handle.engine));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_submit_srt_datagram(
    handle: *mut SrtlaEngineHandle,
    data: *const u8,
    len: usize,
) -> i32 {
    if data.is_null() || len == 0 {
        return -1;
    }
    // SAFETY: caller owns a readable buffer of len bytes for this call.
    let mut packet = Some(unsafe { std::slice::from_raw_parts(data, len) }.to_vec());
    let submitter = {
        let Some(mut engine) = with_engine(handle) else {
            return -1;
        };
        if !engine.running {
            engine.last_error = CString::new("engine is not running").expect("literal CString");
            return -4;
        }
        if let Some(runner) = engine.runner.as_ref() {
            runner.submitter()
        } else if engine
            .input
            .push(packet.take().expect("packet is available"))
        {
            engine.last_error = CString::new("").expect("empty CString");
            return 0;
        } else {
            engine.last_error = CString::new("input queue backpressure").expect("literal CString");
            return -2;
        }
    };

    let result = submitter.submit(packet.take().expect("runner packet is available"));
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    match result {
        Ok(()) => {
            engine.last_error = CString::new("").expect("empty CString");
            0
        }
        Err(EmbeddedSubmitError::Backpressure) => {
            engine.last_error = CString::new("input queue backpressure").expect("literal CString");
            -2
        }
        Err(EmbeddedSubmitError::Stopped) => {
            engine.last_error = CString::new("embedded runner stopped").expect("literal CString");
            -4
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_receive_srt_datagram(
    handle: *mut SrtlaEngineHandle,
    data: *mut u8,
    capacity: usize,
    timeout_ms: u32,
) -> i32 {
    if data.is_null() || capacity == 0 {
        return -1;
    }
    let receiver = {
        let Some(mut engine) = with_engine(handle) else {
            return -1;
        };
        if !engine.running {
            return 0;
        }
        if let Some(runner) = engine.runner.as_ref() {
            runner.receiver()
        } else {
            if let Some(len) = engine.output.front_len()
                && len > capacity
            {
                engine.last_error =
                    CString::new("receive buffer too small").expect("literal CString");
                return -3;
            }
            let packet = engine.output.pop(timeout_ms);
            let Some(packet) = packet else {
                return 0;
            };
            if packet.len() > capacity {
                engine.last_error =
                    CString::new("receive buffer too small").expect("literal CString");
                return -3;
            }
            // SAFETY: caller provides capacity writable bytes.
            unsafe {
                ptr::copy_nonoverlapping(packet.as_ptr(), data, packet.len());
            }
            return packet.len() as i32;
        }
    };
    // Waiting for SRTLA feedback happens outside the engine state lock.  The
    // libsrt send thread can therefore submit media while its receive thread is
    // parked in this callback.
    let packet = receiver.receive(Duration::from_millis(timeout_ms as u64));
    let Some(packet) = packet else {
        return 0;
    };
    if packet.len() > capacity {
        if let Some(mut engine) = with_engine(handle) {
            engine.last_error = CString::new("receive buffer too small").expect("literal CString");
        }
        return -3;
    }
    // SAFETY: caller provides capacity writable bytes.
    unsafe {
        ptr::copy_nonoverlapping(packet.as_ptr(), data, packet.len());
    }
    packet.len() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_set_link_enabled(
    handle: *mut SrtlaEngineHandle,
    link_id: u64,
    enabled: bool,
) -> i32 {
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    if !engine.config.links.iter().any(|link| link.id == link_id) {
        return -2;
    }
    let already_enabled = engine
        .config
        .links
        .iter()
        .find(|link| link.id == link_id)
        .is_some_and(|link| link.enabled);
    if engine.running
        && !enabled
        && already_enabled
        && engine
            .config
            .links
            .iter()
            .filter(|candidate| candidate.enabled)
            .count()
            <= 1
    {
        return -3;
    }
    {
        let Some(link) = engine
            .config
            .links
            .iter_mut()
            .find(|link| link.id == link_id)
        else {
            return -2;
        };
        let reactivated = enabled && !link.enabled;
        if reactivated {
            // A reactivated adapter must warm up with fresh scheduler state;
            // never publish a stale target or delivered-rate sample.
            link.capacity_ready = false;
            link.target_bps = 0;
            link.delivered_bps = 0;
            link.quality_percent = 0;
            link.state = "Warming".to_string();
        }
        if !enabled {
            link.capacity_ready = false;
            link.delivered_bps = 0;
            link.state = "Standby".to_string();
        }
        link.enabled = enabled;
    }
    // A profile may have started while no adapter was present.  Keep the
    // output alive in "Waiting for network" and lazily create the embedded
    // runner as soon as the first usable adapter is enabled.
    if engine.running
        && engine.runner.is_none()
        && engine
            .config
            .links
            .iter()
            .any(|candidate| candidate.address.parse::<std::net::IpAddr>().is_ok())
    {
        engine.start_runner();
    }
    if let Some(runner) = engine.runner.as_ref()
        && !runner.set_link_enabled(link_id, enabled)
    {
        engine.last_error =
            CString::new("link control queue backpressure").expect("literal CString");
        return -2;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_update_adapters(
    handle: *mut SrtlaEngineHandle,
    adapters_json: *const c_char,
) -> i32 {
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    if adapters_json.is_null() {
        return -1;
    }
    // Accept either a bare array or the documented {"links": [...]} object.
    let text = unsafe { CStr::from_ptr(adapters_json) }.to_string_lossy();
    let parsed = serde_json::from_str::<Vec<EngineLinkConfig>>(&text)
        .or_else(|_| serde_json::from_str::<EngineConfig>(&text).map(|c| c.links));
    let Ok(mut links) = parsed else {
        engine.last_error = CString::new("invalid adapter JSON").expect("literal CString");
        return -2;
    };
    for link in &mut links {
        if let Some(old) = engine.config.links.iter().find(|old| old.id == link.id) {
            link.enabled = old.enabled;
        } else {
            // New interfaces are opt-in so a hot-plug event cannot silently
            // increase the aggregate bitrate or expose a metered link.
            link.enabled = false;
        }
    }
    engine.config.links = links;
    if engine.running
        && engine.runner.is_none()
        && engine
            .config
            .links
            .iter()
            .any(|link| link.address.parse::<std::net::IpAddr>().is_ok())
    {
        engine.start_runner();
    }
    if let Some(runner) = engine.runner.as_ref() {
        let links = engine
            .config
            .links
            .iter()
            .filter_map(|link| {
                link.address.parse().ok().map(|address| EmbeddedLink {
                    id: link.id,
                    label: link.label.clone(),
                    address,
                    enabled: link.enabled,
                })
            })
            .collect();
        if !runner.update_links(links) {
            engine.last_error =
                CString::new("adapter update queue backpressure").expect("literal CString");
            return -2;
        }
    }
    0
}

#[derive(Debug, Deserialize)]
struct LinkStatsUpdate {
    id: u64,
    #[serde(default)]
    connected: bool,
    #[serde(default)]
    payload_eligible: bool,
    #[serde(default)]
    capacity_ready: bool,
    #[serde(default)]
    target_bps: u64,
    #[serde(default)]
    quality_percent: u8,
    #[serde(default)]
    rtt_ms: u32,
    #[serde(default)]
    loss_permille: u32,
    #[serde(default)]
    nak_count: u64,
    #[serde(default)]
    used_bps: u64,
    #[serde(default)]
    delivered_bps: u64,
    #[serde(default)]
    state: String,
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_update_link_stats(
    handle: *mut SrtlaEngineHandle,
    stats_json: *const c_char,
) -> i32 {
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    if stats_json.is_null() {
        return -1;
    }
    let text = unsafe { CStr::from_ptr(stats_json) }.to_string_lossy();
    let parsed = serde_json::from_str::<Vec<LinkStatsUpdate>>(&text).or_else(|_| {
        serde_json::from_str::<EngineConfig>(&text).map(|c| {
            c.links
                .into_iter()
                .map(|l| LinkStatsUpdate {
                    id: l.id,
                    connected: l.connected,
                    payload_eligible: l.payload_eligible,
                    capacity_ready: l.capacity_ready,
                    target_bps: l.target_bps,
                    quality_percent: l.quality_percent,
                    rtt_ms: l.rtt_ms,
                    loss_permille: l.loss_permille,
                    nak_count: l.nak_count,
                    used_bps: l.used_bps,
                    delivered_bps: l.delivered_bps,
                    state: l.state,
                })
                .collect()
        })
    });
    let Ok(updates) = parsed else {
        engine.last_error = CString::new("invalid link stats JSON").expect("literal CString");
        return -2;
    };
    for update in updates {
        if let Some(link) = engine
            .config
            .links
            .iter_mut()
            .find(|link| link.id == update.id)
        {
            link.connected = update.connected;
            link.payload_eligible = update.payload_eligible;
            link.capacity_ready = update.capacity_ready;
            link.target_bps = update.target_bps;
            link.quality_percent = update.quality_percent;
            link.rtt_ms = update.rtt_ms;
            link.loss_permille = update.loss_permille;
            link.nak_count = update.nak_count;
            link.used_bps = update.used_bps;
            link.delivered_bps = update.delivered_bps;
            link.state = update.state;
        }
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_update_srt_stats(
    handle: *mut SrtlaEngineHandle,
    stats: *const SrtlaSrtStats,
) -> i32 {
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    if stats.is_null() {
        return -1;
    }
    // SAFETY: the caller supplies a readable structure and advertises its
    // byte size before any field beyond `struct_size` is consumed.
    let struct_size = unsafe { (*stats).struct_size } as usize;
    if struct_size < std::mem::size_of::<SrtlaSrtStats>() {
        return -1;
    }
    // SAFETY: the size check above establishes the complete layout.
    let stats = unsafe { *stats };
    engine.srt_stats = SrtTransportStats {
        sampled_at_ms: stats.sampled_at_ms,
        bandwidth_bps: stats.bandwidth_bps,
        send_rate_bps: stats.send_rate_bps,
        sent_unique_bytes: stats.sent_unique_bytes,
        retransmitted_bytes: stats.retransmitted_bytes,
        dropped_bytes: stats.dropped_bytes,
        send_buffer_ms: stats.send_buffer_ms,
        send_buffer_packets: stats.send_buffer_packets,
        packets_in_flight: stats.packets_in_flight,
        rtt_ms: stats.rtt_ms,
        latency_ms: stats.latency_ms,
        sender_loss_packets: stats.sender_loss_packets,
    };
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_set_audio_bitrate(
    handle: *mut SrtlaEngineHandle,
    audio_bps: u64,
) -> i32 {
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    engine.audio_bps = audio_bps;
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_set_video_bitrate(
    handle: *mut SrtlaEngineHandle,
    video_bps: u64,
) -> i32 {
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    engine.current_video_bps = video_bps.max(1);
    engine.abr.set_current_bps(video_bps);
    engine.last_abr_decision = AbrDecision::default();
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_set_max_video_bitrate(
    handle: *mut SrtlaEngineHandle,
    max_video_bps: u64,
) -> i32 {
    if max_video_bps == 0 {
        return -1;
    }
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    engine.abr.set_max_bps(max_video_bps);
    // Invalidate the previous target so snapshots cannot display a value
    // above the live cap before the next controller sample.
    engine.last_abr_decision = AbrDecision::default();
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_set_session_state(
    handle: *mut SrtlaEngineHandle,
    state: u32,
    error: *const c_char,
) -> i32 {
    if state > SESSION_STOPPED {
        return -1;
    }
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    let previous_state = engine.session_state;
    engine.session_state = state;
    if state != SESSION_CONNECTED {
        engine.srt_stats = SrtTransportStats::default();
        engine.last_abr_decision = AbrDecision::default();
    }
    if previous_state != state {
        let current_video_bps = engine.current_video_bps;
        engine.abr.reset_for_bitrate(current_video_bps);
    }
    engine.session_error = if error.is_null() {
        String::new()
    } else {
        // SAFETY: caller promises a NUL-terminated string for the duration of the call.
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    };
    if state == SESSION_FATAL {
        engine.last_error = CString::new(engine.session_error.clone())
            .unwrap_or_else(|_| CString::new("SRT session failure").expect("literal CString"));
    }
    0
}

/// Tick the ABR controller and return the bitrate that should be applied to
/// the video encoder. The call is intentionally separate from the stats
/// snapshot so a headless output can drive the 20 ms feedback loop without a
/// dock.
#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_apply_abr(handle: *mut SrtlaEngineHandle, now_ms: u64) -> u64 {
    let Some(mut engine) = with_engine(handle) else {
        return 0;
    };
    engine.sync_runner_stats();
    engine.apply_abr(now_ms).applied_bps
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_copy_stats_json(
    handle: *mut SrtlaEngineHandle,
    output: *mut c_char,
    capacity: usize,
) -> usize {
    let Some(mut engine) = with_engine(handle) else {
        return 0;
    };
    engine.sync_runner_stats();
    let json = serde_json::to_vec(&engine.snapshot()).unwrap_or_else(|_| b"{}".to_vec());
    let required = json.len() + 1;
    if output.is_null() || capacity == 0 {
        return required;
    }
    let count = json.len().min(capacity.saturating_sub(1));
    // SAFETY: caller provides capacity writable bytes.
    unsafe {
        ptr::copy_nonoverlapping(json.as_ptr() as *const c_char, output, count);
        *output.add(count) = 0;
    }
    required
}

#[unsafe(no_mangle)]
pub extern "C" fn srtla_engine_last_error(handle: *mut SrtlaEngineHandle) -> *const c_char {
    let Some(engine) = with_engine(handle) else {
        return ptr::null();
    };
    let value = engine.last_error.to_string_lossy().into_owned();
    drop(engine);
    LAST_ERROR_VIEW.with(|slot| {
        *slot.borrow_mut() = CString::new(value)
            .unwrap_or_else(|_| CString::new("engine error").expect("literal CString"));
        slot.borrow().as_ptr()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::{Arc, Barrier};
    use std::time::Instant;

    #[test]
    fn bounded_abi_queue_reports_backpressure() {
        let config = CString::new(r#"{"receiver_host":"example","receiver_port":5000,"links":[{"id":1,"label":"a"},{"id":2,"label":"b"}],"abr":{"max_bps":8000000}}"#).unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(srtla_engine_start(handle), 0);
        let packet = [0x42u8; 8];
        for _ in 0..ENGINE_INPUT_CAPACITY {
            assert_eq!(
                srtla_engine_submit_srt_datagram(handle, packet.as_ptr(), packet.len()),
                0
            );
        }
        assert_eq!(
            srtla_engine_submit_srt_datagram(handle, packet.as_ptr(), packet.len()),
            -2
        );
        assert_eq!(srtla_engine_set_link_enabled(handle, 1, false), 0);
        assert_eq!(srtla_engine_set_link_enabled(handle, 2, false), -3);
        srtla_engine_stop(handle);
        srtla_engine_destroy(handle);
    }

    #[test]
    fn feedback_wait_does_not_block_media_submission() {
        let config = CString::new(
            r#"{"receiver_host":"127.0.0.1","receiver_port":65534,"links":[{"id":1,"label":"loopback","address":"127.0.0.1"}]}"#,
        )
        .unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(srtla_engine_start(handle), 0);

        let rendezvous = Arc::new(Barrier::new(2));
        let receive_rendezvous = rendezvous.clone();
        let handle_address = handle as usize;
        let receiver = std::thread::spawn(move || {
            let mut buffer = [0u8; 1500];
            receive_rendezvous.wait();
            srtla_engine_receive_srt_datagram(
                handle_address as *mut SrtlaEngineHandle,
                buffer.as_mut_ptr(),
                buffer.len(),
                300,
            )
        });

        rendezvous.wait();
        std::thread::sleep(Duration::from_millis(25));
        let packet = [0x42u8; 16];
        let started = Instant::now();
        assert_eq!(
            srtla_engine_submit_srt_datagram(handle, packet.as_ptr(), packet.len()),
            0
        );
        let submit_elapsed = started.elapsed();
        assert!(
            submit_elapsed < Duration::from_millis(150),
            "feedback wait blocked media submission for {submit_elapsed:?}"
        );

        assert_eq!(receiver.join().unwrap(), 0);
        srtla_engine_stop(handle);
        srtla_engine_destroy(handle);
    }

    #[test]
    fn new_adapters_are_opt_in_and_stats_are_nul_terminated() {
        let config = CString::new(r#"{"links":[{"id":1,"label":"old"}]}"#).unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert!(!handle.is_null());
        let adapters = CString::new(r#"[{"id":1,"label":"old"},{"id":2,"label":"new"}]"#).unwrap();
        assert_eq!(srtla_engine_update_adapters(handle, adapters.as_ptr()), 0);
        let needed = srtla_engine_copy_stats_json(handle, ptr::null_mut(), 0);
        let mut out = vec![0 as c_char; needed];
        assert_eq!(
            srtla_engine_copy_stats_json(handle, out.as_mut_ptr(), out.len()),
            needed
        );
        assert_eq!(out[needed - 1], 0);
        srtla_engine_destroy(handle);
    }

    #[test]
    fn link_stats_feed_capacity_snapshot_without_overriding_admin_choice() {
        let config = CString::new(r#"{"links":[{"id":1,"label":"a","enabled":true},{"id":2,"label":"b","enabled":false}]}"#).unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert_eq!(srtla_engine_start(handle), 0);
        let stats = CString::new(r#"[{"id":1,"connected":true,"capacity_ready":true,"target_bps":4000000,"quality_percent":92,"rtt_ms":80,"loss_permille":3,"used_bps":1200000,"delivered_bps":1100000,"state":"Live"}]"#).unwrap();
        assert_eq!(srtla_engine_update_link_stats(handle, stats.as_ptr()), 0);
        let needed = srtla_engine_copy_stats_json(handle, ptr::null_mut(), 0);
        let mut out = vec![0 as c_char; needed];
        srtla_engine_copy_stats_json(handle, out.as_mut_ptr(), out.len());
        let json = unsafe { CStr::from_ptr(out.as_ptr()) }.to_string_lossy();
        assert!(json.contains("4000000"));
        assert!(json.contains(r#""delivered_bps":1100000"#));
        assert!(json.contains("\"admin_enabled\":true"));
        assert_eq!(srtla_engine_set_link_enabled(handle, 2, true), 0);
        assert_eq!(srtla_engine_set_link_enabled(handle, 1, false), 0);
        assert_eq!(srtla_engine_set_link_enabled(handle, 2, false), -3);
        srtla_engine_stop(handle);
        srtla_engine_destroy(handle);
    }

    #[test]
    fn abr_abi_returns_emergency_floor_when_all_enabled_links_are_down() {
        let config = CString::new(r#"{"links":[{"id":1,"label":"a","enabled":true}],"abr":{"min_bps":500000,"start_bps":1500000,"max_bps":8000000}}"#).unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(srtla_engine_start(handle), 0);
        assert_eq!(srtla_engine_apply_abr(handle, 1_000), 500_000);
        srtla_engine_destroy(handle);
    }

    #[test]
    fn maximum_video_bitrate_can_be_lowered_while_running() {
        let config = CString::new(
            r#"{"links":[{"id":1,"label":"a","enabled":true}],"abr":{"min_bps":500000,"start_bps":8000000,"max_bps":20000000}}"#,
        )
        .unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(srtla_engine_start(handle), 0);
        let link_stats = CString::new(
            r#"[{"id":1,"connected":true,"payload_eligible":true,"capacity_ready":true,"target_bps":20000000,"state":"Live"}]"#,
        )
        .unwrap();
        assert_eq!(
            srtla_engine_update_link_stats(handle, link_stats.as_ptr()),
            0
        );
        assert_eq!(
            srtla_engine_set_session_state(handle, SESSION_CONNECTED, ptr::null()),
            0
        );
        assert_eq!(srtla_engine_set_video_bitrate(handle, 8_000_000), 0);

        assert_eq!(srtla_engine_set_max_video_bitrate(handle, 3_000_000), 0);
        assert_eq!(srtla_engine_apply_abr(handle, 1_000), 3_000_000);
        assert_eq!(srtla_engine_set_max_video_bitrate(handle, 0), -1);
        srtla_engine_destroy(handle);
    }

    #[test]
    fn srt_stats_drives_end_to_end_rtt_backoff() {
        let config = CString::new(
            r#"{"links":[{"id":1,"label":"a","enabled":true}],"abr":{"min_bps":500000,"start_bps":8000000,"max_bps":20000000}}"#,
        )
        .unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(srtla_engine_start(handle), 0);
        let link_stats = CString::new(
            r#"[{"id":1,"connected":true,"payload_eligible":true,"capacity_ready":true,"target_bps":10000000,"delivered_bps":4000000,"state":"Live"}]"#,
        )
        .unwrap();
        assert_eq!(
            srtla_engine_update_link_stats(handle, link_stats.as_ptr()),
            0
        );
        assert_eq!(
            srtla_engine_set_session_state(handle, SESSION_CONNECTED, ptr::null()),
            0
        );
        assert_eq!(srtla_engine_set_video_bitrate(handle, 8_000_000), 0);

        let stats = SrtlaSrtStats {
            struct_size: std::mem::size_of::<SrtlaSrtStats>() as u32,
            sampled_at_ms: 20,
            bandwidth_bps: 6_000_000,
            send_rate_bps: 8_128_000,
            sent_unique_bytes: 20_000,
            rtt_ms: 450,
            latency_ms: 2_000,
            ..Default::default()
        };
        assert_eq!(srtla_engine_update_srt_stats(handle, &stats), 0);
        assert_eq!(srtla_engine_apply_abr(handle, 20), 7_100_000);

        let needed = srtla_engine_copy_stats_json(handle, ptr::null_mut(), 0);
        let mut out = vec![0 as c_char; needed];
        srtla_engine_copy_stats_json(handle, out.as_mut_ptr(), out.len());
        let json = unsafe { CStr::from_ptr(out.as_ptr()) }.to_string_lossy();
        assert!(json.contains(r#""link_capacity_bps":10000000"#));
        assert!(json.contains(r#""srt_capacity_bps":0"#));
        assert!(json.contains(r#""estimated_capacity_bps":0"#));
        assert!(json.contains(r#""srt_stats_ready":true"#));
        assert!(json.contains(r#""srt_rtt_ms":450"#));
        assert!(json.contains(r#""abr_state":"Heavy congestion""#));

        let short = SrtlaSrtStats {
            struct_size: 4,
            ..Default::default()
        };
        assert_eq!(srtla_engine_update_srt_stats(handle, &short), -1);
        srtla_engine_destroy(handle);
    }

    #[test]
    fn session_state_requires_both_srt_and_an_eligible_link_for_live() {
        let config = CString::new(
            r#"{"receiver_host":"example","receiver_port":5000,"links":[{"id":1,"label":"a","enabled":true}],"abr":{"start_bps":1500000,"max_bps":8000000}}"#,
        )
        .unwrap();
        let handle = srtla_engine_create(config.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(srtla_engine_start(handle), 0);

        fn stats(handle: *mut SrtlaEngineHandle) -> String {
            let needed = srtla_engine_copy_stats_json(handle, ptr::null_mut(), 0);
            let mut out = vec![0 as c_char; needed];
            assert_eq!(
                srtla_engine_copy_stats_json(handle, out.as_mut_ptr(), out.len()),
                needed
            );
            unsafe { CStr::from_ptr(out.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        }

        assert_eq!(
            srtla_engine_set_session_state(handle, SESSION_CONNECTED, ptr::null()),
            0
        );
        let json = stats(handle);
        assert!(json.contains(r#""srt_session_state":"Connected""#));
        assert!(json.contains(r#""srt_connected":true"#));
        assert!(json.contains(r#""state":"Reconnecting""#));

        let link_stats = CString::new(
            r#"[{"id":1,"connected":true,"payload_eligible":true,"capacity_ready":true,"target_bps":2000000,"state":"Live"}]"#,
        )
        .unwrap();
        assert_eq!(
            srtla_engine_update_link_stats(handle, link_stats.as_ptr()),
            0
        );
        assert!(stats(handle).contains(r#""state":"Live""#));

        let error = CString::new("receiver rejected authentication").unwrap();
        assert_eq!(
            srtla_engine_set_session_state(handle, SESSION_RECONNECTING, error.as_ptr()),
            0
        );
        let json = stats(handle);
        assert!(json.contains(r#""srt_session_state":"Reconnecting""#));
        assert!(json.contains("receiver rejected authentication"));
        assert!(json.contains(r#""state":"Reconnecting""#));

        srtla_engine_stop(handle);
        srtla_engine_destroy(handle);
    }
}
