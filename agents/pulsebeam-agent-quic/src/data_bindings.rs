use std::{
    collections::BTreeMap,
    num::{NonZeroU16, NonZeroU64},
};

use pulsebeam_core::fabric::{
    ExtensionKind, Profile, WireError,
    packet::{Agreement, Offer, VideoDdEmission, VideoDdReception},
};
use pulsebeam_proto::{
    data_endpoint::Format,
    state::{QuicDataV1Parameters, QuicVideoDd, validation::QuicRtpBinding},
};

use crate::records::PacketFraming;

// Association capability, not a packet extension ID. Legacy registries ignore
// this supported-only bit when intersecting packet extensions.
const DIRECTIONAL_VIDEO_DD: u16 = 1 << 15;
const DATA_RECORD_DD: u16 = 1 << 14;

/// Wire capabilities advertised over authenticated control. The binding owner
/// supplies the profile and required features from the authorized source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FabricOffer {
    pub framing: u8,
    pub major: u8,
    pub min_revision: u8,
    pub max_revision: u8,
    pub supported: u16,
}

impl FabricOffer {
    pub const CURRENT: Self = Self {
        framing: PacketFraming::VERSION,
        major: pulsebeam_core::fabric::MAJOR,
        min_revision: 0,
        max_revision: pulsebeam_core::fabric::REGISTRY_REVISION,
        supported: Offer::current(Profile::Opus, 0).supported
            | Offer::current(Profile::H264, 0).supported
            | Offer::current(Profile::Data, 0).supported
            | DIRECTIONAL_VIDEO_DD,
    };

    pub const fn decode([framing, major, min_revision, max_revision, a, b]: [u8; 6]) -> Self {
        Self {
            framing,
            major,
            min_revision,
            max_revision,
            supported: u16::from_be_bytes([a, b]),
        }
    }

    pub const fn encode(self) -> [u8; 6] {
        let [a, b] = self.supported.to_be_bytes();
        [
            self.framing,
            self.major,
            self.min_revision,
            self.max_revision,
            a,
            b,
        ]
    }

    pub const fn supports_directional_video_dd(self, peer: Self) -> bool {
        self.framing == peer.framing
            && self.major == peer.major
            && self.supported & peer.supported & DIRECTIONAL_VIDEO_DD != 0
    }

    /// Enable the association capability only when the endpoint implements
    /// the DATA_V1 record and extension projection in both directions.
    pub const fn with_data_record_dd(mut self) -> Self {
        self.supported |= DATA_RECORD_DD;
        self
    }

    pub const fn supports_data_record_dd(self, peer: Self) -> bool {
        self.framing == peer.framing
            && self.major == peer.major
            && self.supported & peer.supported & DATA_RECORD_DD != 0
    }

    /// Bind a logical DATA_V1 resource. A descriptor-capable resource may
    /// carry descriptor-free records too; legacy resources cannot carry DD.
    pub fn bind_data(mut self, peer: Self, record_dd: bool) -> Result<FabricBinding, WireError> {
        let required = if record_dd {
            if !self.supports_data_record_dd(peer) {
                return Err(WireError::RequiredFeature);
            }
            ExtensionKind::DependencyDescriptor.bit()
        } else {
            0
        };
        // Capture time and delivery coordinates remain fields of the record.
        self.supported &= required;
        self.bind(peer, Profile::Data, required)
    }

    const fn packet_offer(self, required: u16) -> Offer {
        Offer {
            major: self.major,
            min_revision: self.min_revision,
            max_revision: self.max_revision,
            supported: self.supported,
            required,
        }
    }

    /// Match the installed publication's encoding rather than inferring DD
    /// emission from the association's receive capabilities.
    pub fn bind_rtp_publisher(
        self,
        peer: Self,
        profile: Profile,
        emits_video_dd: Option<bool>,
    ) -> Result<FabricBinding, WireError> {
        match emits_video_dd {
            None if profile == Profile::Opus => self.bind(peer, profile, 0),
            Some(emission) if profile.is_video() => {
                let emission = if emission {
                    QuicVideoDd::Emit
                } else {
                    QuicVideoDd::Absent
                };
                self.bind_video_direction(peer, profile, emission, QuicVideoDd::Accept)
            }
            _ => Err(WireError::RequiredFeature),
        }
    }

    /// Select the receiver's wire contract. `Require` is installed by the
    /// publisher only after checking that the source guarantees DD.
    pub fn bind_rtp_receiver(
        self,
        peer: Self,
        profile: Profile,
        reception: Option<QuicVideoDd>,
    ) -> Result<FabricBinding, WireError> {
        match reception {
            None if profile == Profile::Opus => self.bind(peer, profile, 0),
            Some(QuicVideoDd::Absent) => {
                self.bind_video_direction(peer, profile, QuicVideoDd::Absent, QuicVideoDd::Absent)
            }
            Some(QuicVideoDd::Accept)
                if profile.is_video() && self.supports_directional_video_dd(peer) =>
            {
                self.bind(peer, profile, 0)
            }
            Some(QuicVideoDd::Require) => {
                self.bind_video_direction(peer, profile, QuicVideoDd::Emit, QuicVideoDd::Require)
            }
            _ => Err(WireError::RequiredFeature),
        }
    }

    /// `self` describes the publisher and `peer` the receiver for this direction.
    pub fn bind_video_direction(
        self,
        peer: Self,
        profile: Profile,
        emission: QuicVideoDd,
        reception: QuicVideoDd,
    ) -> Result<FabricBinding, WireError> {
        if !self.supports_directional_video_dd(peer) {
            return Err(WireError::RequiredFeature);
        }
        let emission = match emission {
            QuicVideoDd::Absent => VideoDdEmission::None,
            QuicVideoDd::Emit => VideoDdEmission::EveryPacket,
            _ => return Err(WireError::RequiredFeature),
        };
        let reception = match reception {
            QuicVideoDd::Absent => VideoDdReception::None,
            QuicVideoDd::Accept => VideoDdReception::Accepted,
            QuicVideoDd::Require => VideoDdReception::Required,
            _ => return Err(WireError::RequiredFeature),
        };
        Ok(FabricBinding {
            framing: PacketFraming::negotiate(peer.framing).map_err(|_| WireError::Version)?,
            agreement: Agreement::negotiate_video(
                profile,
                self.packet_offer(0),
                emission,
                peer.packet_offer(0),
                reception,
            )?,
        })
    }

    pub fn bind(
        self,
        peer: Self,
        profile: Profile,
        required: u16,
    ) -> Result<FabricBinding, WireError> {
        if self.framing != peer.framing {
            return Err(WireError::Version);
        }
        Ok(FabricBinding {
            framing: PacketFraming::negotiate(peer.framing).map_err(|_| WireError::Version)?,
            agreement: Agreement::negotiate(
                profile,
                self.packet_offer(required),
                peer.packet_offer(0),
            )?,
        })
    }
}

/// Maps a negotiated codec format, never its session-local RTP payload type.
pub fn rtp_profile(mime_type: &str) -> Option<Profile> {
    match mime_type {
        "audio/opus" => Some(Profile::Opus),
        "video/h264" => Some(Profile::H264),
        "video/av1" => Some(Profile::Av1),
        "video/vp8" => Some(Profile::Vp8),
        "video/vp9" => Some(Profile::Vp9),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FabricBinding {
    pub framing: PacketFraming,
    pub agreement: Agreement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataBinding {
    pub id: NonZeroU16,
    pub incarnation: NonZeroU64,
    pub format: Format,
    pub record_dd: bool,
}

impl DataBinding {
    pub fn from_parameters(parameters: QuicDataV1Parameters) -> Option<Self> {
        Some(Self {
            id: NonZeroU16::new(u16::try_from(parameters.binding_id).ok()?)?,
            incarnation: NonZeroU64::new(parameters.incarnation)?,
            format: parameters.format()?,
            record_dd: parameters.record_dd,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadBinding {
    Data(DataBinding),
    Rtp {
        id: NonZeroU16,
        incarnation: NonZeroU64,
    },
}

impl From<QuicRtpBinding<'_>> for PayloadBinding {
    fn from(binding: QuicRtpBinding<'_>) -> Self {
        Self::Rtp {
            id: binding.id(),
            incarnation: binding.incarnation(),
        }
    }
}

impl BindingId for PayloadBinding {
    fn binding_id(&self) -> NonZeroU16 {
        match self {
            Self::Data(data) => data.id,
            Self::Rtp { id, .. } => *id,
        }
    }
}

pub trait BindingId {
    fn binding_id(&self) -> NonZeroU16;
}

impl BindingId for DataBinding {
    fn binding_id(&self) -> NonZeroU16 {
        self.id
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum InstallError {
    Capacity,
    Reused,
}

/// One sender's binding namespace within an admitted association. Only the
/// execution owner may install an accepted binding; packet input is read-only.
pub struct Bindings<B, T> {
    capacity: usize,
    used: Box<[u64]>,
    live: BTreeMap<NonZeroU16, (B, T)>,
}

pub type DataBindings<T> = Bindings<DataBinding, T>;

impl<B: BindingId + Copy + Eq, T> Bindings<B, T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: vec![0; 1024].into_boxed_slice(),
            live: BTreeMap::new(),
        }
    }

    pub fn install(&mut self, binding: B, execution: T) -> Result<(), InstallError> {
        let binding_id = binding.binding_id();
        let id = usize::from(binding_id.get());
        let bit = 1u64 << (id % 64);
        let word = self.used.get_mut(id / 64).ok_or(InstallError::Capacity)?;
        if *word & bit != 0 {
            return Err(InstallError::Reused);
        }
        if self.live.len() >= self.capacity {
            return Err(InstallError::Capacity);
        }
        *word |= bit;
        self.live.insert(binding_id, (binding, execution));
        Ok(())
    }

    pub fn resolve(&self, id: NonZeroU16) -> Option<(B, &T)> {
        self.live
            .get(&id)
            .map(|(binding, execution)| (*binding, execution))
    }

    pub fn retire(&mut self, binding: B) -> Option<T> {
        let id = binding.binding_id();
        if self.resolve(id)?.0 != binding {
            return None;
        }
        self.live.remove(&id).map(|(_, execution)| execution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_proto::state::DataV1Delivery;

    fn binding(id: u16, incarnation: u64) -> DataBinding {
        DataBinding::from_parameters(QuicDataV1Parameters {
            binding_id: id.into(),
            incarnation,
            version: 1,
            delivery: DataV1Delivery::Reliable as i32,
            record_dd: false,
        })
        .unwrap()
    }

    #[test]
    fn directional_dd_requires_both_associations_to_advertise_the_contract() {
        let legacy = FabricOffer::decode([1, 1, 0, 1, 0x03, 0xfc]);
        let directional = FabricOffer {
            supported: legacy.supported | DIRECTIONAL_VIDEO_DD,
            ..legacy
        };
        assert_eq!(directional.encode(), [1, 1, 0, 1, 0x83, 0xfc]);
        for (publisher, receiver) in [(legacy, directional), (directional, legacy)] {
            assert_eq!(
                publisher.bind_video_direction(
                    receiver,
                    Profile::Vp9,
                    QuicVideoDd::Emit,
                    QuicVideoDd::Require
                ),
                Err(WireError::RequiredFeature),
            );
            assert_eq!(
                publisher.bind_rtp_receiver(receiver, Profile::H264, Some(QuicVideoDd::Require)),
                Err(WireError::RequiredFeature),
            );
            assert_eq!(
                publisher.bind_rtp_publisher(receiver, Profile::H264, Some(true)),
                Err(WireError::RequiredFeature),
            );
            assert_eq!(
                publisher.bind_rtp_publisher(receiver, Profile::H264, None),
                Err(WireError::RequiredFeature),
            );
        }
        let binding = directional
            .bind_video_direction(
                directional,
                Profile::Vp9,
                QuicVideoDd::Emit,
                QuicVideoDd::Require,
            )
            .unwrap();
        assert_eq!(binding.agreement.extension_mask() & DIRECTIONAL_VIDEO_DD, 0);
        assert_eq!(binding.agreement.profile(), Profile::Vp9);
        assert_eq!(
            directional.bind_video_direction(
                directional,
                Profile::Vp9,
                QuicVideoDd::Require,
                QuicVideoDd::Emit
            ),
            Err(WireError::RequiredFeature),
        );
    }

    #[test]
    fn fabric_capabilities_bind_using_the_installed_profile_and_requirements() {
        let old = FabricOffer::decode([1, 1, 0, 0, 0, 12]);
        let current = FabricOffer::CURRENT;
        assert_eq!(current.encode(), [1, 1, 0, 1, 131, 252]);
        let old_data = current.bind(old, Profile::Data, 0x000c).unwrap();
        assert_eq!(old_data.agreement.profile(), Profile::Data);
        assert_eq!(old_data.agreement.revision(), 0);
        assert_eq!(old_data.agreement.extension_mask(), 0x000c);
        assert_eq!(old.bind(current, Profile::Data, 0x000c), Ok(old_data));
        for peer in [old, current] {
            let logical_data = current.bind_data(peer, false).unwrap();
            assert_eq!(logical_data.agreement.extension_mask(), 0);
        }
        assert_eq!(
            current.bind(old, Profile::H264, 0x0100),
            Err(WireError::RequiredFeature)
        );
        assert_eq!(
            current.bind(current, Profile::Data, 0x0100),
            Err(WireError::RequiredFeature)
        );
        assert_eq!(
            current.bind(FabricOffer { framing: 2, ..old }, Profile::Data, 0),
            Err(WireError::Version)
        );
    }

    #[test]
    fn data_dd_agreement_does_not_strip_record_dependencies_or_require_them_per_packet() {
        use pulsebeam_core::fabric::{Extension, FabricPacket, Header, packet::MAX_PACKET_BYTES};
        use pulsebeam_proto::data_record::{self, Record};

        let current = FabricOffer::CURRENT;
        let capable = current.with_data_record_dd();
        assert_eq!(capable.encode(), [1, 1, 0, 1, 195, 252]);
        assert_eq!(
            capable.bind_data(current, true),
            Err(WireError::RequiredFeature)
        );
        assert_eq!(
            current.bind_data(capable, true),
            Err(WireError::RequiredFeature)
        );
        let legacy = capable.bind_data(capable, false).unwrap().agreement;
        let capable = capable.bind_data(capable, true).unwrap().agreement;
        let dd = ExtensionKind::DependencyDescriptor;
        assert_eq!(legacy.extension_mask(), 0);
        assert_eq!(capable.extension_mask(), dd.bit());

        let hint = [0xc0, 0, 7];
        let record = Record::new(
            data_record::Header {
                unit_seq: 42,
                index: 0,
                count: 1,
                unit_len: 1,
                capture: None,
            },
            &[0xab],
            None,
        )
        .unwrap();
        let mut payload = [0; data_record::MAX_RECORD_BYTES];
        let len = record.write(&mut payload).unwrap();
        let header = Header {
            profile: Profile::Data,
            marker: true,
            sequence: 19,
            timestamp: 770,
        };
        let packet = FabricPacket::new(
            header,
            &[Extension {
                kind: dd,
                value: &hint,
            }],
            &payload[..len],
        )
        .unwrap();
        let mut wire = [0; MAX_PACKET_BYTES];
        assert_eq!(
            legacy.write(&packet, &mut wire),
            Err(WireError::RequiredFeature)
        );
        let size = capable.write(&packet, &mut wire).unwrap();
        let restored = capable.decode(&wire[..size]).unwrap();
        assert_eq!(restored.extensions().next().unwrap().value, &hint);
        assert_eq!(
            Record::decode_without_descriptor(restored.payload(), None).unwrap(),
            record
        );
        assert_eq!(
            legacy.decode(&wire[..size]).err(),
            Some(WireError::UnnegotiatedExtension)
        );
        let packet = FabricPacket::new(header, &[], &payload[..len]).unwrap();
        let size = capable.write(&packet, &mut wire).unwrap();
        assert!(
            legacy
                .decode(&wire[..size])
                .unwrap()
                .extensions()
                .next()
                .is_none()
        );
    }

    #[test]
    fn data_and_rtp_share_the_sender_namespace() {
        let data = binding(5, 1);
        let media = PayloadBinding::Rtp {
            id: data.id,
            incarnation: data.incarnation,
        };
        let data = PayloadBinding::Data(data);
        let mut namespace = Bindings::new(2);
        namespace.install(data, "data").unwrap();
        assert_eq!(namespace.retire(media), None);
        assert_eq!(namespace.install(media, "media"), Err(InstallError::Reused));
        assert_eq!(namespace.retire(data), Some("data"));
        assert_eq!(namespace.install(media, "media"), Err(InstallError::Reused));
    }

    #[test]
    fn capacity_and_exact_retirement_never_revive_an_old_binding_id() {
        let first = binding(u16::MAX, 1);
        let second = binding(1, 2);
        let mut receive = DataBindings::new(1);
        let mut send = DataBindings::new(1);
        receive.install(first, "publisher").unwrap();
        send.install(first, "subscriber").unwrap();
        assert_eq!(
            receive.install(second, "other"),
            Err(InstallError::Capacity)
        );
        assert_eq!(receive.retire(binding(u16::MAX, 2)), None);
        assert_eq!(receive.resolve(first.id), Some((first, &"publisher")));
        assert_eq!(receive.retire(first), Some("publisher"));
        assert_eq!(receive.resolve(first.id), None);
        assert_eq!(
            receive.install(binding(u16::MAX, 2), "other"),
            Err(InstallError::Reused)
        );
        receive.install(second, "other").unwrap();
        assert_eq!(send.resolve(first.id), Some((first, &"subscriber")));
    }
}
