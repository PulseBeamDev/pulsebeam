macro_rules! checked_identifier {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name($inner);

        impl $name {
            pub const fn new(value: $inner) -> Option<Self> {
                if value == 0 { None } else { Some(Self(value)) }
            }

            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

checked_identifier!(IngressStream, u32);
checked_identifier!(EgressSlot, u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DataChannel(u16);

impl DataChannel {
    pub const fn new(value: u16) -> Option<Self> {
        Some(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

pub type ChannelId = DataChannel;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DepartureReceipt(u64);

impl DepartureReceipt {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TransmissionId(u64);

impl TransmissionId {
    pub const fn get(self) -> u64 {
        self.0
    }
}
