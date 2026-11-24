//! RTP packets at an installed native media binding, before codec processing.

use std::{num::NonZeroU16, time::Instant};

use pulsebeam_agent_core::{Generation, MediaKind};
use pulsebeam_core::fabric::{Extension, FabricPacket, WireError};

use crate::transport::DatagramAdmission;

/// Opaque lifetime of one installed binding on this client.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Binding {
    pub(crate) generation: Generation,
    pub(crate) id: NonZeroU16,
}

/// A ready RTP publication. Resolve it again after a binding is replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub(crate) binding: Binding,
    pub(crate) kind: MediaKind,
    pub(crate) track_id: String,
    pub(crate) publication_id: String,
    pub(crate) ssrc: u32,
    pub(crate) payload_type: u8,
}

impl Publication {
    pub const fn binding(&self) -> Binding {
        self.binding
    }

    pub fn publication_id(&self) -> &str {
        &self.publication_id
    }

    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub const fn payload_type(&self) -> u8 {
        self.payload_type
    }
}

/// Source packet coordinates. The installed publication, not the caller,
/// supplies the SSRC, payload type and codec profile.
pub struct SendPacket<'a> {
    /// Extended RTP sequence within this publication; its low 16 bits are
    /// the RTP sequence number. Supply a new sequence for each source packet.
    pub sequence: u32,
    pub timestamp: u32,
    pub marker: bool,
    /// Semantic extensions, not SDP numeric IDs. Unnegotiated advisory
    /// metadata may be omitted; unnegotiated dependency/timing facts fail.
    pub extensions: &'a [Extension<'a>],
    pub payload: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendPacketError {
    NotReady,
    /// The installed publication already uses the compatibility packetizer.
    SequenceOwner,
    Invalid(WireError),
    Datagram(DatagramAdmission),
}

/// A complete packet as projected for this subscription. Extension kinds
/// are semantic, independent of the RTP extension IDs used by other legs.
#[derive(Debug)]
pub struct ReceivedPacket {
    pub binding: Binding,
    pub publication_id: String,
    pub kind: MediaKind,
    pub arrived_at: Instant,
    pub ssrc: u32,
    pub payload_type: u8,
    pub(crate) packet: FabricPacket<Box<[u8]>>,
}

impl ReceivedPacket {
    pub fn extended_sequence(&self) -> u32 {
        self.packet.header().sequence
    }

    pub fn sequence(&self) -> u16 {
        let [.., a, b] = self.extended_sequence().to_be_bytes();
        u16::from_be_bytes([a, b])
    }

    pub fn timestamp(&self) -> u32 {
        self.packet.header().timestamp
    }

    pub fn marker(&self) -> bool {
        self.packet.header().marker
    }

    pub fn extensions(&self) -> impl Iterator<Item = Extension<'_>> {
        self.packet.extensions()
    }

    pub fn payload(&self) -> &[u8] {
        self.packet.payload()
    }
}

#[derive(Debug)]
pub enum Receive {
    Packet(ReceivedPacket),
    /// Complete packets discarded by the local raw receive queue. Transport
    /// loss is observable as sequence gaps, not as a deadline-driven event.
    Overflow {
        dropped: u64,
    },
}
