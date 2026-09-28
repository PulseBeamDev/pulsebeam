use alloc::{collections::BTreeMap, vec::Vec};

use pulsebeam_proto::signaling_v1 as wire;

use crate::{DesiredState, MediaKind, MediaSlot, MediaTopology, PlayoutDelay, SlotBinding};

use crate::signaling_v1::CatalogState;

pub(crate) const SIGNALING_LABEL: &str = "v1/sys/signaling";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignalingError {
    #[error("signaling message is malformed")]
    Malformed,
    #[error("signaling message has no payload")]
    MissingPayload,
    #[error("signaling state contains an invalid {0}")]
    Invalid(&'static str),
    #[error("media topology has no negotiated resource for {0:?}")]
    MissingCoordinate(MediaSlot),
    #[error("intent exceeds bounded signaling size")]
    Encode,
}

pub(crate) fn resolved_selectors(
    desired: &DesiredState,
    catalog: &CatalogState,
) -> (
    Vec<Option<alloc::string::String>>,
    Vec<Option<alloc::string::String>>,
) {
    let video = desired
        .video
        .iter()
        .filter_map(|video| video.selector.as_ref())
        .map(|selector| catalog.resolve(selector, MediaKind::Video).map(Into::into))
        .collect();
    let audio = desired
        .audio
        .selected
        .iter()
        .map(|selector| catalog.resolve(selector, MediaKind::Audio).map(Into::into))
        .collect();
    (video, audio)
}

pub(crate) fn encode_v1_intent(
    desired: &DesiredState,
    topology: &MediaTopology,
    coordinates: &BTreeMap<MediaSlot, SlotBinding>,
    catalog: &CatalogState,
    revision: u64,
) -> Result<Vec<u8>, SignalingError> {
    if revision == 0 {
        return Err(SignalingError::Invalid("intent revision"));
    }
    if desired.video.len() > usize::from(topology.remote_video) {
        return Err(SignalingError::Invalid("video receiver capacity"));
    }
    let active: BTreeMap<&str, (&str, bool)> = desired
        .publications
        .iter()
        .map(|publication| {
            (
                publication.slot.as_str(),
                (publication.label.as_str(), publication.active),
            )
        })
        .collect();
    let mut send = Vec::new();
    for (count, prefix, kind) in [
        (topology.local_video, 'v', wire::TrackKind::Video),
        (topology.local_audio, 'a', wire::TrackKind::Audio),
    ] {
        for index in 0..count {
            let name = alloc::format!("{prefix}{index}");
            let Some((label, true)) = active.get(name.as_str()).copied() else {
                continue;
            };
            let slot = match kind {
                wire::TrackKind::Video => MediaSlot::LocalVideo(name),
                _ => MediaSlot::LocalAudio(name),
            };
            let resource = coordinates
                .get(&slot)
                .ok_or(SignalingError::MissingCoordinate(slot))?;
            send.push(wire::LocalTrack {
                sender_index: resource.media_index,
                kind: kind.into(),
                label: label.into(),
            });
        }
    }
    let video = desired
        .video
        .iter()
        .filter_map(|track| {
            let id = track.selector.as_ref().map_or_else(
                || Some(track.track_id.as_str()),
                |selector| catalog.resolve(selector, MediaKind::Video),
            )?;
            Some(wire::VideoTrackIntent {
                track_id: id.into(),
                options: Some(wire::VideoOptions {
                    height: track.height,
                    min_height: track.min_height,
                    min_fps: track.min_fps,
                    priority: track.priority,
                    playout_delay: encode_playout_delay(track.playout_delay),
                }),
            })
        })
        .collect();
    let audio = desired
        .audio
        .pinned
        .iter()
        .map(|id| (id.as_str(), desired.audio.playout_delays.get(id).copied()))
        .chain(desired.audio.selected.iter().filter_map(|selector| {
            catalog
                .resolve(selector, MediaKind::Audio)
                .map(|id| (id, desired.audio.selector_delays.get(selector).copied()))
        }))
        .map(|(id, delay)| wire::AudioTrackIntent {
            track_id: id.into(),
            options: Some(wire::AudioOptions {
                playout_delay: delay.and_then(encode_playout_delay),
            }),
        })
        .collect();
    let message = wire::ClientMessage {
        payload: Some(wire::client_message::Payload::Intent(wire::Intent {
            revision,
            send: Some(wire::SendIntent { tracks: send }),
            receive: Some(wire::ReceiveIntent {
                video: Some(wire::VideoIntent { tracks: video }),
                audio: Some(wire::AudioIntent {
                    tracks: audio,
                    mode: if desired.audio.automatic {
                        wire::AudioMode::Auto
                    } else {
                        wire::AudioMode::ExplicitOnly
                    }
                    .into(),
                }),
            }),
        })),
    };
    pulsebeam_proto::codec::encode_client(&message).map_err(|_| SignalingError::Encode)
}

fn encode_playout_delay(value: PlayoutDelay) -> Option<wire::PlayoutDelay> {
    match value {
        PlayoutDelay::Adaptive => None,
        PlayoutDelay::Fixed { min_ms, max_ms } => Some(wire::PlayoutDelay { min_ms, max_ms }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaDirection, MediaKind, PublicationIntent, VideoSubscription};

    #[test]
    fn v1_intent_uses_all_media_section_indices_and_preserves_desired_options() {
        let topology = MediaTopology {
            local_video: 1,
            local_audio: 1,
            remote_video: 1,
            remote_audio: 1,
        };
        let mut coordinates = BTreeMap::new();
        for (slot, media_index, kind) in [
            (MediaSlot::LocalVideo("v0".into()), 2, MediaKind::Video),
            (MediaSlot::LocalAudio("a0".into()), 4, MediaKind::Audio),
        ] {
            coordinates.insert(
                slot.clone(),
                SlotBinding {
                    slot,
                    mid: "nonnumeric-mid".into(),
                    media_index,
                    kind,
                    direction: MediaDirection::SendOnly,
                },
            );
        }
        let desired = DesiredState {
            publications: alloc::vec![
                PublicationIntent {
                    slot: "v0".into(),
                    label: "front camera".into(),
                    active: true
                },
                PublicationIntent {
                    slot: "a0".into(),
                    label: "mic".into(),
                    active: true
                },
            ],
            video: alloc::vec![VideoSubscription {
                slot: 0,
                track_id: "opaque-remote-id".into(),
                selector: None,
                height: 360,
                min_height: 720,
                min_fps: 15,
                priority: 2,
                playout_delay: PlayoutDelay::Fixed {
                    min_ms: 3000,
                    max_ms: 500,
                },
            }],
            ..DesiredState::default()
        };
        let encoded = encode_v1_intent(
            &desired,
            &topology,
            &coordinates,
            &CatalogState::default(),
            5,
        )
        .unwrap();
        let decoded = pulsebeam_proto::codec::decode_client(&encoded).unwrap();
        let Some(wire::client_message::Payload::Intent(intent)) = decoded.payload else {
            panic!("expected compressed v1 intent")
        };
        assert_eq!(intent.revision, 5);
        let send = intent.send.unwrap().tracks;
        assert_eq!(
            send.iter()
                .map(|track| track.sender_index)
                .collect::<Vec<_>>(),
            alloc::vec![2, 4]
        );
        assert_eq!(send[0].label, "front camera");
        let video = intent.receive.unwrap().video.unwrap().tracks;
        assert_eq!(video[0].track_id, "opaque-remote-id");
        let options = video[0].options.as_ref().unwrap();
        assert_eq!((options.height, options.min_height), (360, 720));
        assert_eq!(options.playout_delay.as_ref().unwrap().min_ms, 3000);
        assert_eq!(
            desired.video[0].playout_delay,
            PlayoutDelay::Fixed {
                min_ms: 3000,
                max_ms: 500
            }
        );
    }

    #[test]
    fn per_track_playout_does_not_leak_to_default_video_or_audio() {
        let topology = MediaTopology {
            local_video: 0,
            local_audio: 0,
            remote_video: 2,
            remote_audio: 2,
        };
        let desired = DesiredState {
            video: alloc::vec![
                VideoSubscription {
                    slot: 0,
                    track_id: "fixed-video".into(),
                    selector: None,
                    height: 720,
                    min_height: 0,
                    min_fps: 0,
                    priority: 0,
                    playout_delay: PlayoutDelay::Fixed {
                        min_ms: 15,
                        max_ms: 14,
                    },
                },
                VideoSubscription {
                    slot: 1,
                    track_id: "default-video".into(),
                    selector: None,
                    height: 360,
                    min_height: 0,
                    min_fps: 0,
                    priority: 0,
                    playout_delay: PlayoutDelay::Adaptive,
                },
            ],
            audio: crate::AudioSubscription {
                pinned: alloc::vec!["default-audio".into(), "fixed-audio".into()],
                selected: Vec::new(),
                automatic: true,
                playout_delays: BTreeMap::from([(
                    "fixed-audio".into(),
                    PlayoutDelay::Fixed {
                        min_ms: 3000,
                        max_ms: 500,
                    },
                )]),
                selector_delays: BTreeMap::new(),
            },
            ..DesiredState::default()
        };
        let encoded = encode_v1_intent(
            &desired,
            &topology,
            &BTreeMap::new(),
            &CatalogState::default(),
            1,
        )
        .unwrap();
        let decoded = pulsebeam_proto::codec::decode_client(&encoded).unwrap();
        let Some(wire::client_message::Payload::Intent(intent)) = decoded.payload else {
            panic!("expected intent");
        };
        let receive = intent.receive.unwrap();
        let video = receive.video.unwrap().tracks;
        assert_eq!(
            video[0]
                .options
                .as_ref()
                .unwrap()
                .playout_delay
                .as_ref()
                .unwrap()
                .min_ms,
            15
        );
        assert!(video[1].options.as_ref().unwrap().playout_delay.is_none());
        let audio = receive.audio.unwrap().tracks;
        assert!(audio[0].options.as_ref().unwrap().playout_delay.is_none());
        assert_eq!(
            audio[1]
                .options
                .as_ref()
                .unwrap()
                .playout_delay
                .as_ref()
                .unwrap()
                .max_ms,
            500
        );
        assert_eq!(
            desired.audio.playout_delays["fixed-audio"],
            PlayoutDelay::Fixed {
                min_ms: 3000,
                max_ms: 500,
            }
        );
    }
}
