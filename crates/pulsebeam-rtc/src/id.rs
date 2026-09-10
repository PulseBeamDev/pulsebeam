use std::fmt;

macro_rules! connection_id {
    ($name:ident, $value:ty) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name($value);

        impl $name {
            #[allow(
                dead_code,
                reason = "connection-owned IDs are allocated by Connection beginning in Plan 02"
            )]
            pub(crate) const fn new(value: $value) -> Option<Self> {
                if value == 0 { None } else { Some(Self(value)) }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }
    };
}

macro_rules! caller_id {
    ($name:ident, $value:ty) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name($value);

        impl $name {
            pub const fn from_value(value: $value) -> Self {
                Self(value)
            }

            pub const fn value(self) -> $value {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }
    };
}

connection_id!(SenderId, u16);
connection_id!(EncodingId, u32);
caller_id!(DataChannelId, u16);
caller_id!(IceTcpFlowId, u64);
caller_id!(FrameId, u64);

impl SenderId {
    pub(crate) const fn value(self) -> u16 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_allocated_ids_are_distinct_values() {
        assert_ne!(SenderId::new(1), SenderId::new(2));
        assert_ne!(EncodingId::new(1), EncodingId::new(2));
        assert_eq!(SenderId::new(0), None);
        assert_eq!(EncodingId::new(0), None);
    }
}
