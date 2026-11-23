use std::{collections::BTreeSet, num::NonZeroU64};

use pulsebeam_core::fabric::{
    Profile,
    wan::{WINDOW_TUPLE_BYTES, Window},
};
use pulsebeam_link::NodeRouteKey;
use serde::{Deserialize, Serialize};

use crate::entity::{ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId, TrackKind};
use crate::route::fabric::local::Emission;

/// Live forward authority, received on the same association on which the key is usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct LiveRouteAck {
    pub room: ProjectRoomKey,
    pub publication: PublicationKey,
    pub incarnation: NonZeroU64,
    pub sequence_anchor: u64,
    pub major: u8,
    pub revision: u8,
    pub emission: Emission,
    pub key: NodeRouteKey,
}

impl LiveRouteAck {
    pub(in crate::control) fn same_source(self, other: Self) -> bool {
        self.room == other.room
            && self.publication == other.publication
            && self.incarnation == other.incarnation
            && self.sequence_anchor == other.sequence_anchor
            && self.emission == other.emission
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct WireLiveRouteAck {
    project: ProjectId,
    room: RoomId,
    publication: PublicationId,
    incarnation: NonZeroU64,
    sequence_anchor: u64,
    major: u8,
    revision: u8,
    pt: u8,
    extensions: u16,
    label: [u8; 4],
}

impl From<LiveRouteAck> for WireLiveRouteAck {
    fn from(ack: LiveRouteAck) -> Self {
        let (pt, extensions) = ack.emission.wire_profile();
        Self {
            project: ack.room.project,
            room: ack.room.room,
            publication: ack.publication.publication_id(),
            incarnation: ack.incarnation,
            sequence_anchor: ack.sequence_anchor,
            major: ack.major,
            revision: ack.revision,
            pt,
            extensions,
            label: ack.key.to_bytes(),
        }
    }
}

impl WireLiveRouteAck {
    fn decode_live(self) -> Option<LiveRouteAck> {
        if self.label == [0; 4] {
            return None;
        }
        let emission =
            Emission::from_wire_profile(self.pt, self.extensions, self.major, self.revision)?;
        let kind = track_kind(emission.profile());
        Some(LiveRouteAck {
            room: ProjectRoomKey {
                project: self.project,
                room: self.room,
            },
            publication: PublicationKey::new(kind, self.publication),
            incarnation: self.incarnation,
            sequence_anchor: self.sequence_anchor,
            major: self.major,
            revision: self.revision,
            emission,
            key: NodeRouteKey::from_bytes(self.label),
        })
    }
}

pub(in crate::control) fn decode(offers: Box<[WireLiveRouteAck]>) -> Option<Box<[LiveRouteAck]>> {
    if offers.len() > super::LIMIT {
        return None;
    }
    let mut labels = BTreeSet::new();
    offers
        .into_iter()
        .map(|offer| {
            if !labels.insert(offer.label) {
                return None;
            }
            offer.decode_live()
        })
        .collect()
}

fn track_kind(profile: Profile) -> TrackKind {
    match profile {
        Profile::Opus => TrackKind::Audio,
        Profile::H264 | Profile::Av1 => TrackKind::Video,
        Profile::Data => TrackKind::Data,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct WanLiveRouteAck {
    pub room: ProjectRoomKey,
    pub publication: PublicationKey,
    pub incarnation: NonZeroU64,
    pub sequence_anchor: u64,
    pub window: Window,
}

/// Only the explicitly selected WAN ownership exchange accepts this image.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct WireWanLiveRouteAck {
    project: ProjectId,
    room: RoomId,
    publication: PublicationId,
    incarnation: NonZeroU64,
    sequence_anchor: u64,
    extensions: u16,
    window: [u8; WINDOW_TUPLE_BYTES],
}

impl From<WanLiveRouteAck> for WireWanLiveRouteAck {
    fn from(ack: WanLiveRouteAck) -> Self {
        Self {
            project: ack.room.project,
            room: ack.room.room,
            publication: ack.publication.publication_id(),
            incarnation: ack.incarnation,
            sequence_anchor: ack.sequence_anchor,
            extensions: ack.window.emission(),
            window: ack.window.to_tuple(),
        }
    }
}

pub(in crate::control) fn decode_wan(
    offers: Box<[WireWanLiveRouteAck]>,
) -> Option<Box<[WanLiveRouteAck]>> {
    if offers.len() > super::LIMIT {
        return None;
    }
    let mut keys = BTreeSet::new();
    offers
        .into_iter()
        .map(|wire| {
            let window = Window::from_tuple(wire.window, wire.extensions).ok()?;
            if !keys.insert(window.ssrc()) {
                return None;
            }
            Some(WanLiveRouteAck {
                room: ProjectRoomKey {
                    project: wire.project,
                    room: wire.room,
                },
                publication: PublicationKey::new(track_kind(window.profile()), wire.publication),
                incarnation: wire.incarnation,
                sequence_anchor: wire.sequence_anchor,
                window,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_core::fabric::{ExtensionKind, MAJOR};

    #[test]
    fn wan_live_ack_pins_the_window_separately_from_the_source_anchor() {
        use std::num::NonZeroU32;
        let ack = WanLiveRouteAck {
            room: ProjectRoomKey {
                project: ProjectId::from_bytes([
                    0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1,
                ]),
                room: RoomId::new("fabric").unwrap(),
            },
            publication: crate::entity::ConnectionId::from_bytes([7; 16])
                .fixture_publication(TrackKind::Video, "video"),
            incarnation: NonZeroU64::MIN,
            sequence_anchor: 0x17fff,
            window: Window::new(
                Profile::H264,
                0x18000,
                3,
                NonZeroU32::new(0x05060708).unwrap(),
                1 << 2,
            )
            .unwrap(),
        };
        let json = serde_json::to_value(WireWanLiveRouteAck::from(ack)).unwrap();
        assert_eq!(
            json["window"],
            serde_json::json!([3, 1, 0, 2, 0, 0, 0, 0, 0, 1, 128, 0, 3, 5, 6, 7, 8])
        );
        assert_eq!(json["extensions"], 4);
        assert_eq!(json["sequence_anchor"], 0x17fff);
        assert!(serde_json::from_value::<WireLiveRouteAck>(json.clone()).is_err());
        let mut extra = json.clone();
        extra["label"] = serde_json::json!([5, 6, 7, 8]);
        assert!(serde_json::from_value::<WireWanLiveRouteAck>(extra).is_err());
        assert_eq!(
            decode_wan(Box::new([serde_json::from_value(json).unwrap()])).as_deref(),
            Some(&[ack][..])
        );
        assert!(decode_wan(Box::new([ack.into(), ack.into()])).is_none());
        let mut invalid = WireWanLiveRouteAck::from(ack);
        invalid.window[2] = 1;
        assert!(decode_wan(Box::new([invalid])).is_none());
    }

    #[test]
    fn live_ack_requires_exact_supported_profile_and_unique_nonzero_labels() {
        let ack = LiveRouteAck {
            room: ProjectRoomKey {
                project: ProjectId::from_bytes([1; 16]),
                room: RoomId::new("fabric").unwrap(),
            },
            publication: crate::entity::ConnectionId::from_bytes([7; 16])
                .fixture_publication(TrackKind::Audio, "audio"),
            incarnation: NonZeroU64::MIN,
            sequence_anchor: u64::from(u32::MAX),
            major: MAJOR,
            revision: 1,
            emission: Emission::new(Profile::Opus, &[ExtensionKind::AudioLevel]).unwrap(),
            key: NodeRouteKey::from_bytes([1, 2, 0, 3]),
        };
        assert_eq!(decode(Box::new([ack.into()])).as_deref(), Some(&[ack][..]));
        assert!(decode(Box::new([ack.into(), ack.into()])).is_none());
        for bad in [
            WireLiveRouteAck {
                revision: 0,
                ..ack.into()
            },
            WireLiveRouteAck {
                pt: 97,
                ..ack.into()
            },
            WireLiveRouteAck {
                extensions: 1 << 15,
                ..ack.into()
            },
            WireLiveRouteAck {
                label: [0; 4],
                ..ack.into()
            },
        ] {
            assert!(decode(Box::new([bad])).is_none());
        }
    }
}
