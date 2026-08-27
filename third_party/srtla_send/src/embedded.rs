//! In-process SRTLA sender endpoint.
//!
//! This is the same scheduler and registration state machine used by the CLI,
//! but its SRT side is a bounded Tokio channel rather than a localhost UDP
//! listener.  Uplink sockets are still real UDP sockets bound to the selected
//! source addresses, so no proxy or loopback data path is involved.

#![allow(clippy::collapsible_if)]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant as StdInstant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use srtla_core::mode::SchedulingMode;
use srtla_core::priority::CriticalWindow;
use srtla_core::registration::SrtlaRegistrationManager;
use srtla_core::selection::link_cc::LinkCcController;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::time::{self, Duration, MissedTickBehavior};

use crate::config::DynamicConfig;
use crate::net::{SourceIpBinder, UplinkBinder};
use crate::sender::{
    ConnIoMap, ReaderHandle, RehomeGate, SequenceTracker, apply_connection_changes,
    create_connections_from_ips, create_uplink_channel, drain_packet_queue, flush_all_batches,
    handle_housekeeping, handle_srt_datagram, handle_uplink_packet, sync_readers,
};
use crate::stats::SharedStats;

const INPUT_CAPACITY: usize = 1024;
const OUTPUT_CAPACITY: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddedLink {
    pub id: u64,
    pub label: String,
    pub address: IpAddr,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug)]
pub struct EmbeddedSenderConfig {
    pub receiver_host: String,
    pub receiver_port: u16,
    pub links: Vec<EmbeddedLink>,
    pub scheduler_classic: bool,
}

#[derive(Debug)]
pub enum EmbeddedSubmitError {
    Backpressure,
    Stopped,
}

/// Cloneable input side of the embedded sender.
///
/// The C ABI takes the engine's state lock only long enough to clone this
/// handle, then submits outside that lock.  A full channel therefore applies
/// backpressure to libsrt without blocking unrelated feedback reads or engine
/// control operations.
#[derive(Clone)]
pub struct EmbeddedSubmitter {
    input: mpsc::Sender<Vec<u8>>,
}

impl EmbeddedSubmitter {
    pub fn submit(&self, packet: Vec<u8>) -> std::result::Result<(), EmbeddedSubmitError> {
        match self.input.try_send(packet) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(packet)) => self
                .input
                .blocking_send(packet)
                .map_err(|_| EmbeddedSubmitError::Stopped),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(EmbeddedSubmitError::Stopped),
        }
    }
}

/// Cloneable output side of the embedded sender.
///
/// Waiting for feedback must not hold the engine's global state lock: libsrt
/// has independent send and receive threads, and serialising them behind the
/// receive callback's timeout throttles the media path.
#[derive(Clone)]
pub struct EmbeddedReceiver {
    output: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
}

impl EmbeddedReceiver {
    pub fn receive(&self, timeout: StdDuration) -> Option<Vec<u8>> {
        let deadline = StdInstant::now() + timeout;
        loop {
            let result = {
                let mut receiver = self.output.lock().ok()?;
                receiver.try_recv()
            };
            match result {
                Ok(packet) => return Some(packet),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) if timeout.is_zero() => return None,
                Err(mpsc::error::TryRecvError::Empty) => {
                    if StdInstant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(StdDuration::from_millis(1));
                }
            }
        }
    }
}

enum Control {
    SetLink { id: u64, enabled: bool },
    UpdateLinks(Vec<EmbeddedLink>),
    Stop,
}

/// A thread-safe, bounded in-process sender handle.
pub struct EmbeddedSender {
    submitter: EmbeddedSubmitter,
    receiver: EmbeddedReceiver,
    control: mpsc::Sender<Control>,
    stats: Arc<Mutex<String>>,
    join: Option<thread::JoinHandle<()>>,
}

impl EmbeddedSender {
    pub fn start(config: EmbeddedSenderConfig) -> Self {
        let (input, input_rx) = mpsc::channel(INPUT_CAPACITY);
        let (output_tx, output_rx) = mpsc::channel(OUTPUT_CAPACITY);
        let (control, control_rx) = mpsc::channel(32);
        let receiver = EmbeddedReceiver {
            output: Arc::new(Mutex::new(output_rx)),
        };
        let stats = Arc::new(Mutex::new("{}".to_string()));
        let stats_thread = stats.clone();
        let join = thread::Builder::new()
            .name("srtla-embedded".to_string())
            .spawn(move || {
                let runtime = match Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        *stats_thread.lock().unwrap_or_else(|p| p.into_inner()) =
                            serde_json::json!({"state":"Error","error":err.to_string()})
                                .to_string();
                        return;
                    }
                };
                if let Err(err) = runtime.block_on(run_embedded(
                    config,
                    input_rx,
                    output_tx,
                    control_rx,
                    stats_thread.clone(),
                )) {
                    *stats_thread.lock().unwrap_or_else(|p| p.into_inner()) =
                        serde_json::json!({"state":"Error","error":err.to_string()}).to_string();
                }
            })
            .ok();
        if join.is_none() {
            *stats.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                serde_json::json!({"state":"Error","error":"failed to spawn embedded SRTLA thread"}).to_string();
        }
        Self {
            submitter: EmbeddedSubmitter { input },
            receiver,
            control,
            stats,
            join,
        }
    }

    pub fn submit(&self, packet: Vec<u8>) -> std::result::Result<(), EmbeddedSubmitError> {
        self.submitter.submit(packet)
    }

    pub fn submitter(&self) -> EmbeddedSubmitter {
        self.submitter.clone()
    }

    pub fn receive(&self, timeout: StdDuration) -> Option<Vec<u8>> {
        self.receiver.receive(timeout)
    }

    pub fn receiver(&self) -> EmbeddedReceiver {
        self.receiver.clone()
    }

    pub fn set_link_enabled(&self, id: u64, enabled: bool) -> bool {
        self.control
            .try_send(Control::SetLink { id, enabled })
            .is_ok()
    }

    pub fn update_links(&self, links: Vec<EmbeddedLink>) -> bool {
        self.control.try_send(Control::UpdateLinks(links)).is_ok()
    }

    pub fn stats_json(&self) -> String {
        self.stats
            .lock()
            .map(|stats| stats.clone())
            .unwrap_or_else(|_| "{}".to_string())
    }

    pub fn stop(&mut self) {
        let _ = self.control.blocking_send(Control::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for EmbeddedSender {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn run_embedded(
    config: EmbeddedSenderConfig,
    mut input_rx: mpsc::Receiver<Vec<u8>>,
    output_tx: mpsc::Sender<Vec<u8>>,
    mut control_rx: mpsc::Receiver<Control>,
    stats: Arc<Mutex<String>>,
) -> Result<()> {
    let binder: Arc<dyn UplinkBinder> = Arc::new(SourceIpBinder);
    let mut configured_links = config.links;
    let mut conn_io = ConnIoMap::new();
    let initial_ips = configured_links
        .iter()
        .filter(|link| link.enabled)
        .map(|link| link.address)
        .collect::<Vec<_>>();
    let initial_result = time::timeout(
        Duration::from_secs(5),
        create_connections_from_ips(
            &initial_ips,
            &config.receiver_host,
            config.receiver_port,
            &binder,
            &mut conn_io,
        ),
    )
    .await;
    let mut connections = match initial_result {
        Ok(connections) => connections,
        Err(_) => {
            conn_io.clear();
            smallvec::SmallVec::new()
        }
    };

    let dynamic = DynamicConfig::new();
    dynamic.set_mode(if config.scheduler_classic {
        SchedulingMode::Classic
    } else {
        SchedulingMode::Enhanced
    });
    let critical_window = CriticalWindow::new();
    let shared_stats = SharedStats::new();
    let mut link_cc_controller = LinkCcController::new();
    let mut reg = SrtlaRegistrationManager::new();
    let mut sequence = SequenceTracker::new();
    let (packet_tx, mut packet_rx) = create_uplink_channel();
    let mut readers: HashMap<u64, ReaderHandle> = HashMap::new();
    sync_readers(&connections, &conn_io, &mut readers, &packet_tx);

    if !connections.is_empty() {
        for (idx, packet) in reg.start_probing(&mut connections, srtla_core::utils::now_ms()) {
            if let Some(conn) = connections.get(idx) {
                if let Some(io) = conn_io.get(&conn.conn_id) {
                    let _ = io.socket.send(&packet).await;
                }
            }
        }
    }

    let (instant_tx, mut instant_rx) = mpsc::unbounded_channel::<(SocketAddr, SmallVec<u8, 64>)>();
    let local_addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut last_client = Some(local_addr);
    let mut last_selected = None;
    let mut all_failed_at = None;
    let mut rehome = RehomeGate::new(true);
    let mut housekeeping = time::interval(Duration::from_secs(1));
    housekeeping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut flush = time::interval(Duration::from_millis(15));
    flush.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(packet) = input_rx.recv() => {
                let snapshot = dynamic.snapshot();
                handle_srt_datagram(&packet, local_addr, &mut connections, &conn_io, &mut last_selected,
                    &mut sequence, &mut last_client, reg.has_connected, &snapshot, &critical_window).await;
                drain_packet_queue(&mut packet_rx, &mut connections, &conn_io, &mut reg, &instant_tx,
                    last_client, &mut sequence, &snapshot, &dynamic).await;
            }
            Some(packet) = packet_rx.recv() => {
                handle_uplink_packet(packet, &mut connections, &conn_io, &mut reg, &instant_tx,
                    last_client, &mut sequence, &dynamic.snapshot(), &dynamic).await;
            }
            Some((_, packet)) = instant_rx.recv() => {
                match output_tx.try_send(packet.to_vec()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        if let Ok(mut out) = stats.lock() {
                            *out = serde_json::json!({
                                "state": "Error",
                                "error": "embedded output queue backpressure"
                            }).to_string();
                        }
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            Some(control) = control_rx.recv() => {
                match control {
                Control::Stop => break,
                Control::SetLink { id, enabled } => {
                    let enabled_count = configured_links.iter().filter(|link| link.enabled).count();
                    let Some(link) = configured_links.iter_mut().find(|link| link.id == id) else { continue; };
                    if !enabled && enabled_count <= 1 { continue; }
                    let address = link.address;
                    link.enabled = enabled;
                    let old_ips: std::collections::HashSet<IpAddr> = connections.iter().map(|conn| conn.local_ip).collect();
                    // Keep existing connections alive as warm standbys, while
                    // adding a newly-enabled adapter to the same session.
                    let mut ips: Vec<IpAddr> = connections.iter().map(|conn| conn.local_ip).collect();
                    if enabled && !ips.contains(&address) { ips.push(address); }
                    let _ = time::timeout(
                        Duration::from_secs(2),
                        apply_connection_changes(&mut connections, &mut conn_io, &ips, &config.receiver_host,
                            config.receiver_port, &mut last_selected, &mut sequence, &binder),
                    ).await;
                    let now = srtla_core::utils::now_ms();
                    for conn in &mut connections {
                        if let Some(link) = configured_links.iter().find(|link| link.address == conn.local_ip) {
                            if conn.admin_enabled != link.enabled {
                                conn.set_admin_enabled(link.enabled, now);
                            }
                        }
                    }
                    sync_readers(&connections, &conn_io, &mut readers, &packet_tx);
                    if old_ips.is_empty() && !connections.is_empty() && !reg.has_connected {
                        for (idx, packet) in reg.start_probing(&mut connections, now) {
                            if let Some(conn) = connections.get(idx) {
                                if let Some(io) = conn_io.get(&conn.conn_id) {
                                    let _ = io.socket.send(&packet).await;
                                }
                            }
                        }
                    } else {
                        for (idx, conn) in connections.iter_mut().enumerate() {
                            if !old_ips.contains(&conn.local_ip)
                                && conn.local_ip == address
                                && enabled
                            {
                                let packet = reg.build_reg2(idx);
                                if let Some(io) = conn_io.get(&conn.conn_id) {
                                    if io.socket.send(&packet).await.is_ok() {
                                        conn.note_sent(now);
                                    }
                                }
                            }
                        }
                    }
                }
                Control::UpdateLinks(links) => {
                    let old_ips: std::collections::HashSet<IpAddr> = connections.iter().map(|conn| conn.local_ip).collect();
                    let ips: Vec<IpAddr> = links.iter()
                        .filter(|link| link.enabled || old_ips.contains(&link.address))
                        .map(|link| link.address)
                        .collect();
                    configured_links = links;
                    let _ = time::timeout(
                        Duration::from_secs(2),
                        apply_connection_changes(&mut connections, &mut conn_io, &ips, &config.receiver_host,
                            config.receiver_port, &mut last_selected, &mut sequence, &binder),
                    ).await;
                    for conn in &mut connections {
                        if let Some(link) = configured_links.iter().find(|link| link.address == conn.local_ip) {
                            if conn.admin_enabled != link.enabled {
                                conn.set_admin_enabled(link.enabled, srtla_core::utils::now_ms());
                            }
                        }
                    }
                    sync_readers(&connections, &conn_io, &mut readers, &packet_tx);
                    // A hot-plugged adapter joins an already-established bond
                    // without restarting the SRT session.  The normal
                    // registration driver only probes at initial startup, so
                    // explicitly issue REG2 and arm the one-shot REG3 gate for
                    // each newly-created uplink.
                    let now = srtla_core::utils::now_ms();
                    if old_ips.is_empty() && !connections.is_empty() && !reg.has_connected {
                        for (idx, packet) in reg.start_probing(&mut connections, now) {
                            if let Some(conn) = connections.get(idx) {
                                if let Some(io) = conn_io.get(&conn.conn_id) {
                                    let _ = io.socket.send(&packet).await;
                                }
                            }
                        }
                    } else {
                        for (idx, conn) in connections.iter_mut().enumerate() {
                            if !old_ips.contains(&conn.local_ip)
                                && configured_links.iter().any(|link| link.address == conn.local_ip && link.enabled)
                            {
                                let packet = reg.build_reg2(idx);
                                if let Some(io) = conn_io.get(&conn.conn_id) {
                                    if io.socket.send(&packet).await.is_ok() {
                                        conn.note_sent(now);
                                    }
                                }
                            }
                        }
                    }
                }
                }
            }
            _ = housekeeping.tick() => {
                // A receiver/DNS outage can make the initial five-second
                // connection attempt return with no uplinks.  Retry from the
                // same configured adapter set without requiring the OBS dock
                // to issue a separate hot-plug notification.
                if connections.is_empty() {
                    let retry_ips: Vec<IpAddr> = configured_links.iter()
                        .filter(|link| link.enabled)
                        .map(|link| link.address)
                        .collect();
                    let retry_result = time::timeout(
                        Duration::from_secs(2),
                        apply_connection_changes(&mut connections, &mut conn_io, &retry_ips,
                            &config.receiver_host, config.receiver_port, &mut last_selected,
                            &mut sequence, &binder),
                    ).await;
                    if retry_result.is_err() {
                        conn_io.clear();
                    }
                    sync_readers(&connections, &conn_io, &mut readers, &packet_tx);
                    if !connections.is_empty() && !reg.has_connected {
                        let now = srtla_core::utils::now_ms();
                        for (idx, packet) in reg.start_probing(&mut connections, now) {
                            if let Some(conn) = connections.get(idx) {
                                if let Some(io) = conn_io.get(&conn.conn_id) {
                                    let _ = io.socket.send(&packet).await;
                                }
                            }
                        }
                    }
                }
                let now_ms = srtla_core::utils::now_ms();
                let _ = handle_housekeeping(&mut connections, &mut conn_io, &mut reg, &mut sequence,
                    &config.receiver_host, dynamic.mode().is_classic(), now_ms,
                    &mut all_failed_at, &mut readers, &packet_tx, &mut rehome).await;

                // Keep embedded-runner telemetry in step with the standalone
                // sender. This operation both runs per-link CC and publishes
                // its capacity estimate, so OBS never receives a zero-filled
                // placeholder snapshot for a live connection.
                shared_stats.update_with_link_cc(
                    &mut connections,
                    &dynamic.snapshot(),
                    None,
                    &mut link_cc_controller,
                    now_ms,
                );
                let snapshot = shared_stats.get();
                if let Ok(mut out) = stats.lock() {
                    *out = serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".to_string());
                }
            }
            _ = flush.tick() => {
                flush_all_batches(&mut connections, &conn_io, &mut sequence).await;
            }
            else => break,
        }
    }
    for (_, reader) in readers {
        reader.handle.abort();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_input_waits_for_capacity_instead_of_dropping_a_packet() {
        let (input, mut input_rx) = mpsc::channel(1);
        let submitter = EmbeddedSubmitter { input };
        assert!(submitter.submit(vec![1]).is_ok());

        let blocked = submitter.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx.send(blocked.submit(vec![2])).unwrap();
        });

        assert!(
            done_rx.recv_timeout(StdDuration::from_millis(25)).is_err(),
            "a full input must apply backpressure"
        );
        assert_eq!(input_rx.blocking_recv(), Some(vec![1]));
        assert!(
            done_rx
                .recv_timeout(StdDuration::from_secs(1))
                .expect("blocked submit should resume")
                .is_ok()
        );
        assert_eq!(input_rx.blocking_recv(), Some(vec![2]));
        worker.join().unwrap();
    }
}
