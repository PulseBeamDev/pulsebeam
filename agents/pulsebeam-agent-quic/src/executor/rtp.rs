//! RTP packets at an installed native media binding, before codec processing.

use std::{
    num::{NonZeroU16, NonZeroUsize},
    time::Instant,
};

use pulsebeam_agent_core::{Generation, MediaKind};
use pulsebeam_core::fabric::{Extension, FabricPacket, WireError};

pub use crate::media::SendPacket;
use crate::{media::jitter, transport::DatagramAdmission};

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
    pub(crate) publishes_h264_dd: bool,
}

impl Publication {
    pub const fn binding(&self) -> Binding {
        self.binding
    }

    pub fn publication_id(&self) -> &str {
        &self.publication_id
    }

    pub const fn publishes_h264_dd(&self) -> bool {
        self.publishes_h264_dd
    }

    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub const fn payload_type(&self) -> u8 {
        self.payload_type
    }
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

    /// Adapt a packet after sequence ordering. Pass the extended sequence and
    /// presentation timestamp supplied by the ordering stage.
    pub fn ordered(
        &self,
        sequence: u64,
        rtp_time: Option<u64>,
    ) -> crate::media::units::OrderedPacket<'_> {
        crate::media::units::OrderedPacket {
            sequence,
            timestamp: self.timestamp(),
            rtp_time,
            arrived_at: self.arrived_at,
            marker: self.marker(),
            payload: self.payload(),
            descriptor: self
                .extensions()
                .find(|extension| {
                    extension.kind == pulsebeam_core::fabric::ExtensionKind::DependencyDescriptor
                })
                .map(|extension| extension.value),
        }
    }

    pub fn payload(&self) -> &[u8] {
        self.packet.payload()
    }
}

impl From<ReceivedPacket> for jitter::Input<Binding, ReceivedPacket> {
    fn from(packet: ReceivedPacket) -> Self {
        let bytes = packet
            .packet
            .retained_body_bytes()
            .saturating_add(packet.publication_id.capacity());
        Self {
            source: packet.binding,
            sequence: packet.extended_sequence(),
            timestamp: packet.timestamp(),
            arrived_at: packet.arrived_at,
            retained_bytes: NonZeroUsize::new(bytes).unwrap_or(NonZeroUsize::MIN),
            packet,
        }
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
