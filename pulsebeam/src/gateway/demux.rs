use pulsebeam_runtime::net;
use pulsebeam_runtime::net::UnifiedSocketReader;

use crate::entity::ParticipantId;
use crate::gateway::ice;
use ahash::{HashMap, HashMapExt};
use std::net::SocketAddr;

type ParticipantKey = ParticipantId;

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
    /// The source of truth: maps a remote ICE ufrag to a participant id.
    ufrag_map: HashMap<Box<[u8]>, ParticipantKey>,
    /// A fast-path cache mapping a remote `SocketAddr` to a known participant id.
    addr_map: HashMap<SocketAddr, ParticipantKey>,
    /// A reverse map from a ufrag to all known addresses, for efficient cleanup.
    ufrag_addrs: HashMap<Box<[u8]>, Vec<SocketAddr>>,
    /// A reverse map from a socket addr to ufrag, for cleanup.
    addr_to_ufrag: HashMap<SocketAddr, Box<[u8]>>,
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            ufrag_map: HashMap::new(),
            addr_map: HashMap::new(),
            ufrag_addrs: HashMap::new(),
            addr_to_ufrag: HashMap::new(),
        }
    }

    /// Registers a participant with their ICE username fragment.
    pub fn register_ice_ufrag(&mut self, ufrag: &[u8], participant_key: ParticipantKey) {
        let boxed_ufrag = ufrag.to_vec().into_boxed_slice();
        self.ufrag_map.insert(boxed_ufrag, participant_key);
    }

    /// Removes a participant and all associated state (ufrag and address mappings).
    pub fn unregister(&mut self, socket: &mut UnifiedSocketReader, ufrag: &[u8]) {
        self.ufrag_map.remove(ufrag);
        if let Some(addrs) = self.ufrag_addrs.remove(ufrag) {
            for addr in addrs {
                self.addr_map.remove(&addr);
                socket.close_peer(&addr);
            }
        }
    }

    /// Routes a packet to the correct participant.
    /// Route a packet to the correct participant key.
    /// Returns `Some(participant_key)` if routed.
    pub fn demux(&mut self, _socket: &mut UnifiedSocketReader, batch: &net::RecvPacketBatch) -> Option<ParticipantKey> {
        let src = batch.src;

        if let Some(key) = self.addr_map.get(&src).cloned() {
            return Some(key);
        }

        if let Some(ufrag_raw) = ice::parse_stun_remote_ufrag_raw(batch.data()) {
            if let Some(key) = self.ufrag_map.get(ufrag_raw).cloned() {
                let boxed_ufrag = ufrag_raw.to_vec().into_boxed_slice();
                self.addr_map.insert(src, key.clone());
                self.addr_to_ufrag.insert(src, boxed_ufrag.clone());
                self.ufrag_addrs.entry(boxed_ufrag).or_default().push(src);
                return Some(key);
            }
        }

        // Unknown participant; if source was previously mapped, forget it.
        if let Some(ufrag) = self.addr_to_ufrag.remove(&src) {
            self.ufrag_addrs.remove(&ufrag);
        }

        None
    }
}


impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}
