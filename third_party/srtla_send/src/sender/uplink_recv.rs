use anyhow::Result;
use smallvec::SmallVec;
use srtla_core::connection::{SrtlaConnection, SrtlaIncoming};
use srtla_core::registration::{RegistrationEvent, SrtlaRegistrationManager};
use srtla_protocol::*;
use tracing::debug;

/// Process one received uplink datagram.
///
/// Updates the connection's protocol state and accumulates the receive-side
/// effects (forward-to-client bytes, ACK/NAK/SRTLA-ACK sequence numbers, a
/// deferred immediate-REG1 send) into a [`SrtlaIncoming`]. Forwarding is
/// always emitted through the shell-owned channel so the same code can use a
/// UDP listener or an embedded in-memory endpoint.
///
/// This used to be an inherent `impl SrtlaConnection` method; it lives in the
/// shell now so the connection type owns no receive-side socket.
#[allow(clippy::too_many_arguments)]
pub async fn process_uplink_packet(
    conn: &mut SrtlaConnection,
    conn_idx: usize,
    reg: &mut SrtlaRegistrationManager,
    data: &[u8],
) -> Result<SrtlaIncoming> {
    let mut incoming = SrtlaIncoming {
        read_any: true,
        ..Default::default()
    };
    let now = srtla_core::utils::now_ms();
    let pt = get_packet_type(data);
    if let Some(pt) = pt {
        if let Some(event) = reg.process_registration_packet(conn_idx, data, now) {
            match event {
                RegistrationEvent::RegNgp => {
                    // Answering REG_NGP with an immediate REG1 is deferred to
                    // the shell: build the packet here (advancing reg state)
                    // and stash it as an effect so this stays free of uplink I/O.
                    if let Some(pkt) = reg.reg1_if_ngp_immediate(conn_idx, now) {
                        incoming.reg1_send = Some(pkt);
                    }
                }
                RegistrationEvent::Reg3 => {
                    // Clear any phantom in-flight packets and NAK state
                    // accumulated during pre-registration data forwarding
                    conn.clear_pre_registration_state(now);
                    conn.connected = true;
                    conn.last_received = Some(now);
                    if conn.reconnection.connection_established_ms == 0 {
                        conn.reconnection.connection_established_ms = now;
                    }
                    conn.reconnection.mark_success(&conn.label);
                }
                RegistrationEvent::RegErr => {
                    conn.connected = false;
                    conn.last_received = None;
                }
                RegistrationEvent::Reg2 => {}
            }
            return Ok(incoming);
        }

        conn.last_received = Some(now);

        if pt == SRT_TYPE_ACK {
            if let Some(ack) = parse_srt_ack(data) {
                incoming.ack_numbers.push(ack);
            }
            let ack_packet = SmallVec::from_slice_copy(data);
            incoming.forward_to_client.push(ack_packet);
        } else if pt == SRT_TYPE_NAK {
            let nak_list = parse_srt_nak(data);
            if !nak_list.is_empty() {
                debug!(
                    "📦 NAK received from {}: {} sequences",
                    conn.label,
                    nak_list.len()
                );
                for seq in nak_list {
                    incoming.nak_numbers.push(seq);
                }
            }
            incoming
                .forward_to_client
                .push(SmallVec::from_slice_copy(data));
        } else if pt == SRTLA_TYPE_ACK {
            let ack_list = parse_srtla_ack(data);
            if !ack_list.is_empty() {
                debug!(
                    "🎯 SRTLA ACK received from {}: {} sequences",
                    conn.label,
                    ack_list.len()
                );
                for seq in ack_list {
                    incoming.srtla_ack_numbers.push(seq);
                }
            }
        } else if pt == SRT_TYPE_HANDSHAKE {
            // The far end declares its TSBPD receive delay in the clear here,
            // and this is the only place we can see it: the scheduler otherwise
            // has to guess a delay budget from its own RTT samples. Only the
            // response is worth reading — an HSREQ crossing the other way is
            // the local caller's *proposal*, whereas the responder has already
            // resolved both sides to max(own, proposed) by the time it answers.
            if let Some(hs) = parse_srt_handshake_latency(data)
                && hs.is_response
                && let Some(rcv_ms) = hs.rcv_ms
            {
                debug!(
                    "{}: SRT peer declared a {}ms receive buffer",
                    conn.label, rcv_ms
                );
                incoming.negotiated_latency_ms = Some(rcv_ms);
            }
            // Sniffing only — the handshake still belongs to the client.
            incoming
                .forward_to_client
                .push(SmallVec::from_slice_copy(data));
        } else if pt == SRTLA_TYPE_KEEPALIVE {
            if conn
                .rtt
                .handle_keepalive_response(data, &conn.label, now)
                .is_some()
            {
                conn.record_rtt_probe();
                // Delivery proof for `stall_deselect`: a completed keepalive
                // round-trip proves this link's path is alive even while no
                // data ACKs are landing. Pairs with the earned-ACK site
                // (see `ack_nak.rs`); together they let a recovered link
                // un-gate itself without the scheduler probing blindly.
                conn.last_ack_or_rtt_sample_ms = now;
            }
        } else {
            incoming
                .forward_to_client
                .push(SmallVec::from_slice_copy(data));
        }
    }
    Ok(incoming)
}
