use pulsebeam_runtime::mailbox::TrySendError;
use pulsebeam_runtime::{mailbox, net};

use crate::entity::ParticipantId;
use crate::gateway::ice;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

pub type ParticipantHandle = mailbox::Sender<net::RecvPacketBatch>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemuxResult {
    Participant(Arc<ParticipantId>),
    Rejected(RejectionReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    /// Packet received from an unknown source address that was not a STUN binding request.
    UnknownSource,
    /// STUN packet could not be parsed or was missing a USERNAME attribute.
    MalformedStun,
    /// STUN packet had a valid format but an unknown USERNAME attribute.
    UnauthorizedIceUfrag,
    /// Packet was too small to be processed.
    PacketTooSmall,
}

/// A UDP demuxer that maps packets to participants based on source address and STUN ufrag.
///
/// This implementation uses two primary mechanisms for routing incoming UDP packets:
/// 1. A fast-path map from `SocketAddr` to `ParticipantId` (`addr_map`). This provides
///    efficient routing for known addresses for non-STUN traffic (DTLS, RTP, RTCP).
/// 2. For any STUN packet, or for non-STUN packets from an unknown address, it inspects
///    the packet to learn the route. STUN binding requests are used to create or update
///    the address mapping.
///
/// Non-STUN packets from unknown addresses are rejected.
pub struct Demuxer {
    /// The source of truth: maps a remote ICE ufrag to a participant.
    ufrag_map: HashMap<Box<[u8]>, ParticipantHandle>,
    /// A fast-path cache mapping a remote `SocketAddr` to a known participant.
    addr_map: HashMap<SocketAddr, ParticipantHandle>,
    /// A reverse map from a participant to their ufrag, for cleanup.
    participant_ufrag: HashMap<Arc<ParticipantId>, Box<[u8]>>,
    /// A reverse map from a ufrag to all known addresses, for efficient cleanup.
    ufrag_addrs: HashMap<Box<[u8]>, Vec<SocketAddr>>,
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            ufrag_map: HashMap::new(),
            addr_map: HashMap::new(),
            participant_ufrag: HashMap::new(),
            ufrag_addrs: HashMap::new(),
        }
    }

    /// Registers a participant with their ICE username fragment.
    pub fn register_ice_ufrag(
        &mut self,
        participant_id: Arc<ParticipantId>,
        ufrag: &[u8],
        participant_handle: ParticipantHandle,
    ) {
        let boxed_ufrag = ufrag.to_vec().into_boxed_slice();
        self.participant_ufrag
            .insert(participant_id, boxed_ufrag.clone());
        self.ufrag_map.insert(boxed_ufrag, participant_handle);
    }

    /// Removes a participant and all associated state (ufrag and address mappings).
    pub fn unregister(&mut self, participant_id: &Arc<ParticipantId>) {
        if let Some(ufrag) = self.participant_ufrag.remove(participant_id) {
            self.ufrag_map.remove(&ufrag);
            // Use the ufrag_addrs map to efficiently clean the addr_map
            if let Some(addrs) = self.ufrag_addrs.remove(&ufrag) {
                for addr in addrs {
                    self.addr_map.remove(&addr);
                }
            }
        }
    }

    /// Routes a packet to the correct participant.
    /// Returns `true` if sent, `false` if dropped (queue full or unknown destination).
    pub fn demux(&mut self, batch: net::RecvPacketBatch) -> bool {
        // 1. RESOLVE DESTINATION
        let participant_handle = if let Some(handle) = self.addr_map.get_mut(&batch.src) {
            handle
        } else if let Some(ufrag) = ice::parse_stun_remote_ufrag_raw(&batch.buf) {
            // New connection establishment (rare path, logging is okay here)
            if let Some(handle) = self.ufrag_map.get_mut(ufrag) {
                tracing::debug!("New connection from ufrag: {:?} -> {}", ufrag, batch.src);

                self.addr_map.insert(batch.src, handle.clone());
                let key = ufrag.to_vec().into_boxed_slice();
                self.ufrag_addrs.entry(key).or_default().push(batch.src);

                handle
            } else {
                // Packet from unknown ufrag
                tracing::trace!("Dropped: unregistered stun binding from {}", batch.src);
                return false;
            }
        } else {
            // Packet from unknown source without ufrag
            tracing::trace!("Dropped: unknown source {}", batch.src);
            return false;
        };

        // 2. SEND (NON-BLOCKING)
        // We use try_send. If the channel is full, we drop the packet immediately.
        // We do NOT log warnings here as it would thrash I/O during a DDoS or congestion.
        match participant_handle.try_send(batch) {
            Ok(_) => true,
            Err(TrySendError::Full(_)) => {
                // PRODUCTION TIP: Increment a fast counter here (e.g., metrics crate)
                // metrics::increment_counter!("gateway_drops_full");
                false
            }
            Err(TrySendError::Closed(_)) => {
                // The participant actor has died or disconnected.
                // You might want to trigger a cleanup of self.addr_map here eventually.
                false
            }
        }
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}
