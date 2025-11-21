use pulsebeam_runtime::mailbox::TrySendError;
use pulsebeam_runtime::{mailbox, net, rt};

use crate::entity::ParticipantId;
use crate::gateway::ice;
use crate::participant;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

pub type ParticipantHandle = mailbox::Sender<net::RecvPacket>;

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

    /// Determines the owner of an incoming UDP packet.
    pub async fn demux(&mut self, pkt: net::RecvPacket) {
        let participant_handle = if let Some(participant_handle) = self.addr_map.get_mut(&pkt.src) {
            participant_handle
        } else if let Some(ufrag) = ice::parse_stun_remote_ufrag_raw(&pkt.buf) {
            if let Some(participant_handle) = self.ufrag_map.get_mut(ufrag) {
                tracing::debug!("found connection from ufrag: {:?} -> {}", ufrag, pkt.src,);
                self.addr_map.insert(pkt.src, participant_handle.clone());
                let key = ufrag.to_vec().into_boxed_slice();
                self.ufrag_addrs.entry(key).or_default().push(pkt.src);
                participant_handle
            } else {
                tracing::trace!(
                    "dropped a packet from {} due to unregistered stun binding: {:?}",
                    pkt.src,
                    ufrag
                );
                return;
            }
        } else {
            tracing::trace!(
                "dropped a packet from {} due to unexpected message flow from an unknown source",
                pkt.src
            );
            return;
        };

        // participant_handle.send(pkt).await;
        if let Err(TrySendError::Full(_)) = participant_handle.try_send(pkt) {
            tracing::warn!("gateway -> participant: full queue, dropping");
            // downstream has later stages of work.. let them have a chance.
            rt::yield_now().await;
        }
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}
