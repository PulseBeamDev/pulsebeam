use ahash::AHashMap;
use pulsebeam_runtime::net;
use pulsebeam_runtime::net::UnifiedSocketReader;

use crate::gateway::ice;
use std::net::SocketAddr;

pub type ParticipantHandle = pulsebeam_runtime::sync::mpsc::Sender<net::RecvPacketBatch>;

// ── Inline ICE ufrag key ───────────────────────────────────────────────────────
//
// RFC 5245 permits ufrags of 4–256 printable ASCII characters.  In practice
// (str0m, Chrome, Firefox) they are always ≤ 32 bytes.  Storing the ufrag
// inline eliminates the `Box<[u8]>` heap allocation and the subsequent pointer
// chase on every STUN lookup (which happens for every new ICE candidate address).
//
// Size: 1 (len) + 32 (data) = 33 bytes → within one hash map bucket entry.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct IceUfrag {
    len: u8,
    bytes: [u8; 32],
}

impl IceUfrag {
    /// Returns `None` if `b` is longer than 32 bytes (would indicate a
    /// malformed/adversarial STUN packet; we simply reject it).
    #[inline]
    fn from_slice(b: &[u8]) -> Option<Self> {
        if b.len() > 32 {
            return None;
        }
        let mut bytes = [0u8; 32];
        bytes[..b.len()].copy_from_slice(b);
        Some(IceUfrag { len: b.len() as u8, bytes })
    }

    #[inline]
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

// ── Demuxer ────────────────────────────────────────────────────────────────────

/// A UDP demuxer that routes incoming packets to participants via ICE ufrag /
/// source-address lookup.
///
/// ## Cache design
///
/// **Hot path** (every non-STUN packet after initial ICE handshake):
///   `addr_map: AHashMap<SocketAddr, u16>` — compact 30-byte entries (vs the
///   previous 36-byte entries with a fat `Sender` value).  AHashMap uses
///   AHash rather than SipHash, which is 3–5× faster for integer-like keys
///   such as SocketAddr on x86-64.
///
/// **Sender slab** (indexed by the u16 from `addr_map`):
///   `senders: Vec<Option<ParticipantHandle>>` — all 300 Arc<Ring<>> pointers
///   live in a single 2.4 KB contiguous region (300 × 8 bytes).  This is
///   small enough to stay permanently hot in L1/L2 across all participants,
///   compared to the previous design where each Sender lived at a random heap
///   address inside the HashMap bucket.
///
/// **ICE/STUN path** (< 10 times per participant lifetime — effectively cold):
///   `ufrag_map: AHashMap<IceUfrag, u16>` — inline 33-byte key, no pointer
///   chase to heap-allocated ufrag bytes.
pub struct Demuxer {
    // ── HOT: one lookup per non-STUN UDP packet ─────────────────────────────
    addr_map: AHashMap<SocketAddr, u16>,

    // ── SENDER SLAB: all Arc<Ring<>> pointers in one contiguous region ──────
    // Capacity grows monotonically; slots are reclaimed via free_slots.
    // At 300 participants: 300 × 8 bytes = 2.4 KB — stays in L1.
    senders: Vec<Option<ParticipantHandle>>,
    free_slots: Vec<u16>,

    // ── WARM: one lookup per new ICE candidate address ───────────────────────
    ufrag_map: AHashMap<IceUfrag, u16>,

    // ── COLD: cleanup bookkeeping (participant leave / handle closed) ────────
    ufrag_addrs: AHashMap<IceUfrag, Vec<SocketAddr>>,
    addr_to_ufrag: AHashMap<SocketAddr, IceUfrag>,
}

impl Demuxer {
    pub fn new() -> Self {
        Self {
            addr_map: AHashMap::new(),
            senders: Vec::new(),
            free_slots: Vec::new(),
            ufrag_map: AHashMap::new(),
            ufrag_addrs: AHashMap::new(),
            addr_to_ufrag: AHashMap::new(),
        }
    }

    /// Allocates a slot index, reusing a freed slot when available.
    fn alloc_slot(&mut self) -> u16 {
        if let Some(slot) = self.free_slots.pop() {
            slot
        } else {
            let idx = self.senders.len();
            self.senders.push(None);
            idx as u16
        }
    }

    /// Registers a participant with their remote ICE username fragment.
    pub fn register_ice_ufrag(&mut self, ufrag: &[u8], handle: ParticipantHandle) {
        let Some(key) = IceUfrag::from_slice(ufrag) else {
            tracing::warn!(len = ufrag.len(), "ICE ufrag exceeds 32 bytes; ignoring");
            return;
        };
        let slot = self.alloc_slot();
        self.senders[slot as usize] = Some(handle);
        self.ufrag_map.insert(key, slot);
    }

    /// Removes a participant and all associated routing state.
    pub fn unregister(&mut self, socket: &mut UnifiedSocketReader, ufrag: &[u8]) {
        let Some(key) = IceUfrag::from_slice(ufrag) else { return };
        self.unregister_by_key(socket, &key);
    }

    fn unregister_by_key(&mut self, socket: &mut UnifiedSocketReader, key: &IceUfrag) {
        if let Some(slot) = self.ufrag_map.remove(key) {
            self.senders[slot as usize] = None;
            self.free_slots.push(slot);
        }
        if let Some(addrs) = self.ufrag_addrs.remove(key) {
            for addr in addrs {
                self.addr_map.remove(&addr);
                self.addr_to_ufrag.remove(&addr);
                socket.close_peer(&addr);
            }
        }
    }

    /// Routes a packet to the correct participant.
    /// Returns `true` if dispatched, `false` if dropped.
    #[inline]
    pub fn demux(&mut self, socket: &mut UnifiedSocketReader, batch: net::RecvPacketBatch) -> bool {
        let src = batch.src;

        // Fast path: source address already mapped to a slot index.
        let slot = if let Some(&s) = self.addr_map.get(&src) {
            s
        } else if let Some(ufrag_raw) = ice::parse_stun_remote_ufrag_raw(&batch.buf) {
            // STUN binding request: learn which participant this address belongs to.
            let Some(key) = IceUfrag::from_slice(ufrag_raw) else {
                return false;
            };
            let Some(&s) = self.ufrag_map.get(&key) else {
                return false;
            };
            // Cache the address → slot mapping for all future packets.
            self.addr_map.insert(src, s);
            self.addr_to_ufrag.insert(src, key.clone());
            self.ufrag_addrs.entry(key).or_default().push(src);
            s
        } else {
            return false;
        };

        // Slab lookup: one bounds-checked Vec index, no pointer chain.
        let Some(sender) = self.senders.get(slot as usize).and_then(Option::as_ref) else {
            // Stale mapping (rapid join/leave race); evict the address entry.
            self.addr_map.remove(&src);
            return false;
        };

        if sender.try_send(batch).is_err() {
            // Participant channel closed; clean up all state for this ufrag.
            if let Some(key) = self.addr_to_ufrag.get(&src).cloned() {
                tracing::info!(ufrag = ?key.as_bytes(), "Participant handle closed, cleaning up");
                self.unregister_by_key(socket, &key);
            }
            return false;
        }

        true
    }
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

