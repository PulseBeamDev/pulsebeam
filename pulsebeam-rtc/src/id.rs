macro_rules! identifier {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name($inner);

        impl $name {
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }

            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

identifier!(ConnectionId, u64);
identifier!(MediaSectionId, u16);
identifier!(StreamId, u32);
identifier!(ChannelId, u16);
identifier!(PacketId, u64);
identifier!(SendId, u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_round_trip_their_values() {
        assert_eq!(ConnectionId::new(1).get(), 1);
        assert_eq!(MediaSectionId::new(2).get(), 2);
        assert_eq!(StreamId::new(3).get(), 3);
        assert_eq!(ChannelId::new(4).get(), 4);
        assert_eq!(PacketId::new(5).get(), 5);
        assert_eq!(SendId::new(6).get(), 6);
    }

    #[test]
    fn identifiers_remain_distinct_types() {
        let packet = PacketId::new(17);
        let send = SendId::new(17);

        assert_eq!(packet.get(), send.get());
    }
}
