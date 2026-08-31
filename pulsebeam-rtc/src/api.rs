pub use crate::peer::{
    DependencyRewrite, ExtendedMediaSequence, ExtendedRtpTimestamp, ForwardingLatency,
    H264NalMetadata, MediaExtensions, MediaPacket, MediaPacketError, MediaRewrite, MediaSemantics,
    TransitMediaPacket, VideoLayersAllocation, VideoSpatialLayerAllocation, VideoStreamAllocation,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceCandidate(String);

impl IceCandidate {
    pub fn new(value: String) -> Option<Self> {
        if value.is_empty() {
            return None;
        }

        Some(Self(value))
    }

    pub fn as_sdp(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_mdns(&self) -> bool {
        let Some((_, value)) = self.0.split_once(':') else {
            return false;
        };
        let mut fields = value.split_ascii_whitespace();
        let _foundation = fields.next();
        let _component = fields.next();
        let _protocol = fields.next();
        let _priority = fields.next();
        let address = fields.next();
        let _port = fields.next();
        let typ = fields.next();
        let _kind = fields.next();
        matches!(typ, Some("typ")) && address.is_some_and(|address| address.ends_with(".local"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
    Application,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaDirection {
    SendOnly,
    ReceiveOnly,
    Inactive,
    Bidirectional,
}
use std::{net::SocketAddr, time::Instant};

use crate::{ChannelId, Codec, TransportProtocol};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatagramProtocol {
    Udp,
    Tcp,
}

impl DatagramProtocol {
    pub(crate) const fn into_transport(self) -> TransportProtocol {
        match self {
            Self::Udp => TransportProtocol::Udp,
            Self::Tcp => TransportProtocol::Tcp,
        }
    }

    pub(crate) const fn from_transport(protocol: TransportProtocol) -> Self {
        match protocol {
            TransportProtocol::Udp => Self::Udp,
            TransportProtocol::Tcp => Self::Tcp,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct IngressDatagram {
    pub(crate) protocol: DatagramProtocol,
    pub(crate) source: SocketAddr,
    pub(crate) destination: SocketAddr,
    pub(crate) bytes: Vec<u8>,
}

impl IngressDatagram {
    pub fn new(
        protocol: DatagramProtocol,
        source: SocketAddr,
        destination: SocketAddr,
        bytes: Vec<u8>,
    ) -> Self {
        debug_assert!(!bytes.is_empty(), "an ingress datagram contains bytes");
        Self {
            protocol,
            source,
            destination,
            bytes,
        }
    }

    pub const fn protocol(&self) -> DatagramProtocol {
        self.protocol
    }

    pub const fn source(&self) -> SocketAddr {
        self.source
    }

    pub const fn destination(&self) -> SocketAddr {
        self.destination
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

macro_rules! opaque_handle {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name(pub(crate) u32);

        impl $name {
            pub(crate) const fn new(value: u32) -> Self {
                debug_assert!(value != 0, "opaque RTC handles are nonzero");
                Self(value)
            }
        }
    };
}

opaque_handle!(IngressStream);
opaque_handle!(EgressSlot);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedCodec {
    pub(crate) name: String,
    clock_rate: u32,
    channels: Option<u8>,
}

impl NegotiatedCodec {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    pub const fn channels(&self) -> Option<u8> {
        self.channels
    }
}

impl From<&Codec> for NegotiatedCodec {
    fn from(codec: &Codec) -> Self {
        Self {
            name: codec.name().to_owned(),
            clock_rate: codec.clock_rate(),
            channels: codec.channels(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegotiatedMedia {
    pub(crate) ingress: Option<IngressStream>,
    pub(crate) egress: Option<EgressSlot>,
    pub(crate) mid: String,
    pub(crate) rid: Option<String>,
    pub(crate) kind: MediaKind,
    pub(crate) direction: MediaDirection,
    pub(crate) codecs: Box<[NegotiatedCodec]>,
}

impl NegotiatedMedia {
    pub const fn ingress(&self) -> Option<IngressStream> {
        self.ingress
    }

    pub const fn egress(&self) -> Option<EgressSlot> {
        self.egress
    }

    pub fn mid(&self) -> &str {
        &self.mid
    }

    pub fn rid(&self) -> Option<&str> {
        self.rid.as_deref()
    }

    pub const fn kind(&self) -> MediaKind {
        self.kind
    }

    pub const fn direction(&self) -> MediaDirection {
        self.direction
    }

    pub fn codecs(&self) -> &[NegotiatedCodec] {
        &self.codecs
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcNegotiation {
    pub(crate) answer: String,
    pub(crate) media: Box<[NegotiatedMedia]>,
}

impl RtcNegotiation {
    pub fn answer(&self) -> &str {
        &self.answer
    }

    pub fn media(&self) -> &[NegotiatedMedia] {
        &self.media
    }
}
