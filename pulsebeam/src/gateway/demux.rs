use pulsebeam_runtime::net;
use pulsebeam_runtime::net::UnifiedSocketReader;

use crate::gateway::ice;
use ahash::{HashMap, HashMapExt};
use std::net::SocketAddr;

pub type ParticipantHandle = pulsebeam_runtime::sync::mpsc::Sender<net::RecvPacketBatch>;

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
    /// A reverse map from a ufrag to all known addresses, for efficient cleanup.
    ufrag_addrs: HashMap<Box<[u8]>, Vec<SocketAddr>>,
    /// A reverse map from a socket addr to ufrag, for cleanup.
    addr_to_ufrag: HashMap<SocketAddr, Box<[u8]>>,
    /// High-water fill ratio seen across all participant channels since the last
    /// `reset_pressure()` call. 0.0 = all queues empty, 1.0 = a queue is full.
    max_pressure: f64,
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            ufrag_map: HashMap::new(),
            addr_map: HashMap::new(),
            ufrag_addrs: HashMap::new(),
            addr_to_ufrag: HashMap::new(),
            max_pressure: 0.0,
        }
    }

    /// Registers a participant with their ICE username fragment.
    pub fn register_ice_ufrag(&mut self, ufrag: &[u8], participant_handle: ParticipantHandle) {
        let boxed_ufrag = ufrag.to_vec().into_boxed_slice();
        self.ufrag_map.insert(boxed_ufrag, participant_handle);
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
    /// Returns `true` if sent, `false` if dropped
    pub fn demux(&mut self, socket: &mut UnifiedSocketReader, batch: net::RecvPacketBatch) -> bool {
        let src = batch.src;

        let handle = if let Some(h) = self.addr_map.get_mut(&src) {
            h
        } else if let Some(ufrag_raw) = ice::parse_stun_remote_ufrag_raw(batch.data()) {
            if let Some(h) = self.ufrag_map.get_mut(ufrag_raw) {
                let boxed_ufrag = ufrag_raw.to_vec().into_boxed_slice();

                // Link address to handle and ufrag
                self.addr_map.insert(src, h.clone());
                self.addr_to_ufrag.insert(src, boxed_ufrag.clone());
                self.ufrag_addrs.entry(boxed_ufrag).or_default().push(src);

                h
            } else {
                return false;
            }
        } else {
            return false;
        };

        if let Err(_) = handle.try_send(batch) {
            // Handle is closed! Clean up everything related to this participant.
            if let Some(ufrag) = self.addr_to_ufrag.get(&src).cloned() {
                tracing::info!("Participant handle closed, cleaning up ufrag: {:?}", ufrag);
                self.unregister(socket, &ufrag);
            }
            return false;
        }

        // Track downstream fill pressure so the gateway read loop can decide
        // whether to yield and give participants time to drain their queues.
        let fill = handle.fill_ratio();
        if fill > self.max_pressure {
            self.max_pressure = fill;
        }

        true
    }

    /// Returns the highest fill ratio observed across all participant channels
    /// since the last `reset_pressure()` call.
    pub fn pressure(&self) -> f64 {
        self.max_pressure
    }

    /// Computes the current live fill ratio across all registered participant
    /// channels by reading their head/tail atomics directly. Use this after
    /// yielding to check whether participants have had enough time to drain
    /// before pulling more data from the socket.
    pub fn current_pressure(&self) -> f64 {
        self.ufrag_map
            .values()
            .map(|h| h.fill_ratio())
            .fold(0.0f64, f64::max)
    }

    /// Resets the high-water pressure mark. Call this at the start of each
    /// gateway read cycle so the next batch of sends gets a fresh measurement.
    pub fn reset_pressure(&mut self) {
        self.max_pressure = 0.0;
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}
