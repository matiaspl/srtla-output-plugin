#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod abr;

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

pub use abr::{AbrConfig, AbrController, AbrDecision, AbrSample, LinkCapacity};

const ENGINE_INPUT_CAPACITY: usize = 1024;
const ENGINE_OUTPUT_CAPACITY: usize = 256;

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
    estimated_capacity_bps: u64,
    recommended_video_bps: u64,
    current_video_bps: u64,
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
        EngineSnapshot {
            receiver_host: self.config.receiver_host.clone(),
            receiver_port: self.config.receiver_port,
            running: self.running,
            state: if !self.last_error.to_string_lossy().is_empty() {
                "Error".to_string()
            } else if !self.running {
                "Idle".to_string()
            } else if self.config.links.iter().any(|l| l.enabled && l.connected) {
                "Live".to_string()
            } else if self.config.links.iter().any(|l| l.enabled) {
                "Reconnecting".to_string()
            } else {
                "Waiting for network".to_string()
            },
            error: self.last_error.to_string_lossy().into_owned(),
            queue_in: self.input.len(),
            queue_out: self.output.len(),
            estimated_capacity_bps: self
                .config
                .links
                .iter()
                .filter(|l| l.enabled && l.payload_eligible && l.capacity_ready)
                .map(|l| l.target_bps)
                .sum(),
            recommended_video_bps: {
                let ready = self
                    .config
                    .links
                    .iter()
                    .any(|l| l.enabled && l.payload_eligible && l.capacity_ready);
                let any_connected = self.config.links.iter().any(|l| l.enabled && l.connected);
                let capacity: u64 = self
                    .config
                    .links
                    .iter()
                    .filter(|l| l.enabled && l.payload_eligible && l.capacity_ready)
                    .map(|l| l.target_bps)
                    .sum();
                if !any_connected {
                    self.abr.config().min_bps
                } else if !ready {
                    self.abr
                        .config()
                        .start_bps
                        .saturating_sub(self.audio_bps)
                        .max(self.abr.config().min_bps)
                } else {
                    let budget = ((capacity as f64) * self.abr.config().safety_margin) as u64;
                    budget
                        .saturating_sub(self.audio_bps)
                        .clamp(self.abr.config().min_bps, self.abr.config().max_bps)
                }
            },
            current_video_bps: self.current_video_bps,
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
            })
            .collect::<Vec<_>>();
        let capacity_ready = links
            .iter()
            .any(|link| link.enabled && link.payload_eligible && link.capacity_ready);
        let all_links_down = !self
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
        });
        self.current_video_bps = decision.applied_bps;
        decision
    }
}

fn with_engine<'a>(handle: *mut SrtlaEngineHandle) -> Option<MutexGuard<'a, Engine>> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: handles are created by create_v1 and consumed only through this ABI.
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
pub extern "C" fn srtla_engine_create_v1(config_json: *const c_char) -> *mut SrtlaEngineHandle {
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
    // SAFETY: ownership is returned by create_v1 and destroy is terminal.
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
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    if !engine.running {
        engine.last_error = CString::new("engine is not running").expect("literal CString");
        return -4;
    }
    // SAFETY: caller owns a readable buffer of len bytes for this call.
    let packet = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    if let Some(runner) = engine.runner.as_ref() {
        match runner.submit(packet) {
            Ok(()) => {
                engine.last_error = CString::new("").expect("empty CString");
                0
            }
            Err(EmbeddedSubmitError::Backpressure) => {
                engine.last_error =
                    CString::new("input queue backpressure").expect("literal CString");
                -2
            }
            Err(EmbeddedSubmitError::Stopped) => {
                engine.last_error =
                    CString::new("embedded runner stopped").expect("literal CString");
                -4
            }
        }
    } else if engine.input.push(packet) {
        engine.last_error = CString::new("").expect("empty CString");
        0
    } else {
        engine.last_error = CString::new("input queue backpressure").expect("literal CString");
        -2
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
    let Some(mut engine) = with_engine(handle) else {
        return -1;
    };
    if !engine.running {
        return 0;
    }
    if let Some(len) = engine.output.front_len()
        && len > capacity
    {
        engine.last_error = CString::new("receive buffer too small").expect("literal CString");
        return -3;
    }
    let packet = if let Some(runner) = engine.runner.as_ref() {
        runner.receive(Duration::from_millis(timeout_ms as u64))
    } else {
        engine.output.pop(timeout_ms)
    };
    let Some(packet) = packet else {
        return 0;
    };
    if packet.len() > capacity {
        engine.last_error = CString::new("receive buffer too small").expect("literal CString");
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
    let reactivated = {
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
            // A reactivated adapter must warm up with a fresh capacity
            // estimate; never let a stale DHCP/session sample raise the
            // aggregate bitrate.
            link.capacity_ready = false;
            link.target_bps = 0;
            link.quality_percent = 0;
            link.state = "Warming".to_string();
        }
        if !enabled {
            link.capacity_ready = false;
            link.state = "Standby".to_string();
        }
        link.enabled = enabled;
        reactivated
    };
    if reactivated {
        engine.abr.freeze_growth_ticks(10);
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
                    capacity_ready: l.capacity_ready,
                    target_bps: l.target_bps,
                    quality_percent: l.quality_percent,
                    rtt_ms: l.rtt_ms,
                    loss_permille: l.loss_permille,
                    nak_count: l.nak_count,
                    used_bps: l.used_bps,
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
            link.capacity_ready = update.capacity_ready;
            link.target_bps = update.target_bps;
            link.quality_percent = update.quality_percent;
            link.rtt_ms = update.rtt_ms;
            link.loss_permille = update.loss_permille;
            link.nak_count = update.nak_count;
            link.used_bps = update.used_bps;
            link.state = update.state;
        }
    }
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
    engine.current_video_bps =
        video_bps.clamp(engine.abr.config().min_bps, engine.abr.config().max_bps);
    0
}

/// Tick the stable ABR controller and return the bitrate that should be
/// applied to the video encoder.  The call is intentionally separate from the
/// stats snapshot so a headless output can drive ABR at 1 Hz without a dock.
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
    engine.last_error.as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn bounded_abi_queue_reports_backpressure() {
        let config = CString::new(r#"{"receiver_host":"example","receiver_port":5000,"links":[{"id":1,"label":"a"},{"id":2,"label":"b"}],"abr":{"max_bps":8000000}}"#).unwrap();
        let handle = srtla_engine_create_v1(config.as_ptr());
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
    fn new_adapters_are_opt_in_and_stats_are_nul_terminated() {
        let config = CString::new(r#"{"links":[{"id":1,"label":"old"}]}"#).unwrap();
        let handle = srtla_engine_create_v1(config.as_ptr());
        assert!(!handle.is_null());
        let adapters = CString::new(r#"[{"id":1,"label":"old"},{"id":2,"label":"new"}]"#).unwrap();
        assert_eq!(srtla_engine_update_adapters(handle, adapters.as_ptr()), 0);
        let needed = srtla_engine_copy_stats_json(handle, ptr::null_mut(), 0);
        let mut out = vec![0i8; needed];
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
        let handle = srtla_engine_create_v1(config.as_ptr());
        assert_eq!(srtla_engine_start(handle), 0);
        let stats = CString::new(r#"[{"id":1,"connected":true,"capacity_ready":true,"target_bps":4000000,"quality_percent":92,"rtt_ms":80,"loss_permille":3,"used_bps":1200000,"state":"Live"}]"#).unwrap();
        assert_eq!(srtla_engine_update_link_stats(handle, stats.as_ptr()), 0);
        let needed = srtla_engine_copy_stats_json(handle, ptr::null_mut(), 0);
        let mut out = vec![0i8; needed];
        srtla_engine_copy_stats_json(handle, out.as_mut_ptr(), out.len());
        let json = unsafe { CStr::from_ptr(out.as_ptr()) }.to_string_lossy();
        assert!(json.contains("4000000"));
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
        let handle = srtla_engine_create_v1(config.as_ptr());
        assert!(!handle.is_null());
        assert_eq!(srtla_engine_start(handle), 0);
        assert_eq!(srtla_engine_apply_abr(handle, 1_000), 500_000);
        srtla_engine_destroy(handle);
    }
}
