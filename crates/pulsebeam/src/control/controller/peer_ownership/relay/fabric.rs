use std::{collections::BTreeSet, num::NonZeroU64};

use pulsebeam_core::fabric::{
    Profile,
    wan::{WINDOW_TUPLE_BYTES, Window},
};
use serde::{Deserialize, Serialize};

use crate::entity::{ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId, TrackKind};

fn track_kind(profile: Profile) -> TrackKind {
    match profile {
        Profile::Opus => TrackKind::Audio,
        Profile::H264 | Profile::Av1 | Profile::Vp8 | Profile::Vp9 => TrackKind::Video,
        Profile::Data => TrackKind::Data,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::control) struct WanLiveRouteAck {
    pub room: ProjectRoomKey,
    pub publication: PublicationKey,
    pub incarnation: NonZeroU64,
    pub sequence_anchor: u64,
    pub offer_cut: Option<NonZeroU64>,
    pub window: Window,
}

/// Wire form shared by WAN ownership exchanges and routed branch leases.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct WireWanLiveRouteAck {
    project: ProjectId,
    room: RoomId,
    publication: PublicationId,
    incarnation: NonZeroU64,
    sequence_anchor: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    offer_cut: Option<NonZeroU64>,
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
            offer_cut: ack.offer_cut,
            extensions: ack.window.emission(),
            window: ack.window.to_tuple(),
        }
    }
}

impl WireWanLiveRouteAck {
    pub(in crate::control) fn decode(self) -> Option<WanLiveRouteAck> {
        let window = Window::from_tuple(self.window, self.extensions).ok()?;
        Some(WanLiveRouteAck {
            room: ProjectRoomKey {
                project: self.project,
                room: self.room,
            },
            publication: PublicationKey::new(track_kind(window.profile()), self.publication),
            incarnation: self.incarnation,
            sequence_anchor: self.sequence_anchor,
            offer_cut: self.offer_cut,
            window,
        })
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
            let ack = wire.decode()?;
            keys.insert(ack.window.ssrc()).then_some(ack)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            offer_cut: None,
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
            serde_json::json!([3, 1, 1, 2, 0, 0, 0, 0, 0, 1, 128, 0, 3, 5, 6, 7, 8])
        );
        assert_eq!(json["extensions"], 4);
        assert_eq!(json["sequence_anchor"], 0x17fff);
        let mut extra = json.clone();
        extra["label"] = serde_json::json!([5, 6, 7, 8]);
        assert!(serde_json::from_value::<WireWanLiveRouteAck>(extra).is_err());
        assert_eq!(
            decode_wan(Box::new([serde_json::from_value(json).unwrap()])).as_deref(),
            Some(&[ack][..])
        );
        assert!(decode_wan(Box::new([ack.into(), ack.into()])).is_none());
        for revision in [0, 2] {
            let mut invalid = WireWanLiveRouteAck::from(ack);
            invalid.window[2] = revision;
            assert!(decode_wan(Box::new([invalid])).is_none());
        }
    }
}
