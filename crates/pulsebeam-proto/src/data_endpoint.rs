//! DATA endpoint negotiation and lifetime identity for FabricPacket #275 §5.4.
//! These are control-owner values, not fields in SCTP DATA fragments.

use core::num::{NonZeroU16, NonZeroU64};

use crate::data_control::{PublisherBinding, SubscriberBinding};

pub const DATA_V1: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Reliable,
    Unreliable,
    Latest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    delivery: Delivery,
}

impl Format {
    /// An absent version cannot select a format, even when the payload resembles DATA_V1.
    pub fn negotiate(version: Option<u8>, delivery: Delivery) -> Option<Self> {
        (version == Some(DATA_V1)).then_some(Self { delivery })
    }

    pub const fn version(self) -> u8 {
        DATA_V1
    }

    pub const fn delivery(self) -> Delivery {
        self.delivery
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Publisher(PublisherBinding),
    Subscriber(SubscriberBinding),
}

/// `A` is the local owner's association incarnation, not a transport address or
/// identity claimed by the peer. `stream` is the physical payload stream;
/// stream zero remains the existing authenticated control channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel<A> {
    pub association: A,
    pub stream: NonZeroU16,
    pub incarnation: NonZeroU64,
}

/// The exact admitted or acknowledged payload binding. The installer compares
/// the complete value, including format and endpoint, before making it live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding<A> {
    pub channel: Channel<A>,
    pub format: Format,
    pub endpoint: Endpoint,
}

impl<A> Binding<A> {
    pub fn reliable_control(&self) -> Option<Endpoint> {
        (self.format.delivery() == Delivery::Reliable).then_some(self.endpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_explicit_and_acknowledgment_matches_the_full_channel_lifetime()
    -> Result<(), &'static str> {
        assert_eq!(Format::negotiate(None, Delivery::Reliable), None);
        assert_eq!(Format::negotiate(Some(0), Delivery::Reliable), None);
        assert_eq!(Format::negotiate(Some(2), Delivery::Reliable), None);
        let format =
            Format::negotiate(Some(DATA_V1), Delivery::Reliable).ok_or("DATA_V1 must negotiate")?;
        assert_eq!(format.version(), DATA_V1);
        let endpoint = Endpoint::Publisher(PublisherBinding {
            source: NonZeroU64::MIN,
        });
        let live = Binding {
            channel: Channel {
                association: 1_u64,
                stream: NonZeroU16::MIN,
                incarnation: NonZeroU64::MIN,
            },
            format,
            endpoint,
        };
        assert_eq!(live.reliable_control(), Some(endpoint));
        for changed in [
            Channel {
                association: 2,
                ..live.channel
            },
            Channel {
                stream: NonZeroU16::MIN.saturating_add(1),
                ..live.channel
            },
            Channel {
                incarnation: NonZeroU64::MIN.saturating_add(1),
                ..live.channel
            },
        ] {
            assert_ne!(
                Binding {
                    channel: changed,
                    ..live
                },
                live
            );
        }
        assert_ne!(
            Binding {
                endpoint: Endpoint::Subscriber(SubscriberBinding {
                    source: NonZeroU64::MIN,
                    branch: NonZeroU64::MIN,
                }),
                ..live
            },
            live
        );
        let suppress_reliable = Binding {
            format: Format {
                delivery: Delivery::Latest,
                ..format
            },
            ..live
        };
        assert_ne!(suppress_reliable, live);
        assert_eq!(suppress_reliable.reliable_control(), None);
        Ok(())
    }
}
