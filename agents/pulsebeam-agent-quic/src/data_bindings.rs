use std::{
    collections::BTreeMap,
    num::{NonZeroU16, NonZeroU64},
};

use pulsebeam_proto::{
    data_endpoint::Format,
    state::{QuicDataV1Parameters, validation::QuicRtpBinding},
};

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
