use std::{
    collections::BTreeMap,
    num::{NonZeroU16, NonZeroU64},
};

use pulsebeam_core::fabric::{
    ExtensionKind, Profile, WireError,
    packet::{Agreement, Offer},
};
use pulsebeam_proto::{
    data_endpoint::Format,
    state::{QuicDataV1Parameters, validation::QuicRtpBinding},
};

use crate::records::PacketFraming;

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
            | Offer::current(Profile::Data, 0).supported,
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
    pub fn bind_publisher(
        mut self,
        peer: Self,
        profile: Profile,
        publishes_h264_dd: bool,
    ) -> Result<FabricBinding, WireError> {
        let dd = ExtensionKind::DependencyDescriptor.bit();
        let required = if publishes_h264_dd {
            if profile != Profile::H264 {
                return Err(WireError::ExtensionProfile);
            }
            dd
        } else {
            if profile == Profile::H264 {
                self.supported &= !dd;
            }
            0
        };
        self.bind(peer, profile, required)
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
}

impl DataBinding {
    pub fn from_parameters(parameters: QuicDataV1Parameters) -> Option<Self> {
        Some(Self {
            id: NonZeroU16::new(u16::try_from(parameters.binding_id).ok()?)?,
            incarnation: NonZeroU64::new(parameters.incarnation)?,
            format: parameters.format()?,
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
        })
        .unwrap()
    }

    #[test]
    fn fabric_capabilities_bind_using_the_installed_profile_and_requirements() {
        let old = FabricOffer::decode([1, 1, 0, 0, 0, 12]);
        let current = FabricOffer::CURRENT;
        assert_eq!(current.encode(), [1, 1, 0, 1, 3, 252]);
        let old_data = current.bind(old, Profile::Data, 0x000c).unwrap();
        assert_eq!(old_data.agreement.profile(), Profile::Data);
        assert_eq!(old_data.agreement.revision(), 0);
        assert_eq!(old_data.agreement.extension_mask(), 0x000c);
        assert_eq!(old.bind(current, Profile::Data, 0x000c), Ok(old_data));
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
