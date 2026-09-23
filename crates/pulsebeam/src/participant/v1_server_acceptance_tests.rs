use super::*;
use crate::control::{NegotiatedResources, Negotiator, controller::ConnectionProfile};
use crate::entity::{self, ParticipantExternalId, RoomExternalId, TrackKind};
use crate::id::ShardId;
use crate::keys::TrackHandle;
use crate::participant::downstream::{PlayoutPolicy, SlotConfig};
use crate::participant::event::test_utils::MockParticipantSink;
use crate::track::TrackMeta;
use pulsebeam_proto::signaling_v1 as v1;
use slotmap::SlotMap;
use str0m::change::SdpOffer;
use str0m::channel::{ChannelConfig, ChannelData, ChannelId};
use str0m::media::{Direction, MediaKind, Mid};
use str0m::{Event, IceCreds, Rtc, RtcConfig};
use tokio::time::Instant;

struct Harness {
    participant: Participant,
    cid: ChannelId,
    sink: MockParticipantSink,
    handles: SlotMap<TrackHandle, ()>,
    video_receiver_mid: Mid,
    video_sender: u32,
    audio_sender: u32,
    recorded: Vec<Vec<u8>>,
}

impl Harness {
    fn new(expiry: Option<i64>) -> Self {
        let mut offer_rtc = RtcConfig::new().build(std::time::Instant::now());
        let mut change = offer_rtc.sdp_api();
        change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
        change.add_channel("signal".to_owned());
        change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let mut offer_sdp = change.apply().unwrap().0.to_sdp_string();
        let mids: Vec<_> = offer_sdp
            .lines()
            .filter_map(|line| line.strip_prefix("a=mid:"))
            .map(str::to_owned)
            .collect();
        for (index, mid) in mids.iter().enumerate() {
            let replacement = format!("media-{index}");
            offer_sdp = offer_sdp.replace(&format!("a=mid:{mid}"), &format!("a=mid:{replacement}"));
            offer_sdp = offer_sdp.replace(&format!(" {mid}"), &format!(" {replacement}"));
        }
        let offer = SdpOffer::from_sdp_string(&offer_sdp).unwrap();
        let (mut rtc, _, resources) = Negotiator::new(Vec::new())
            .create_answer(offer, IceCreds::new(), ConnectionProfile::Native)
            .unwrap();
        assert!(
            resources
                .as_slice()
                .iter()
                .all(|r| r.mid.to_string().parse::<u32>().is_err())
        );
        let resource = |kind, direction| {
            resources
                .as_slice()
                .iter()
                .find(|r| r.kind == kind && r.direction == direction)
                .unwrap()
        };
        let video_receiver_mid = resource(MediaKind::Video, Direction::SendOnly).mid;
        let video_sender = resource(MediaKind::Video, Direction::RecvOnly).media_index;
        let audio_sender = resource(MediaKind::Audio, Direction::RecvOnly).media_index;
        let negotiated = resources.as_slice().to_vec();
        let cid = rtc.direct_api().create_data_channel(ChannelConfig {
            label: "v1/sys/signaling".to_owned(),
            ..Default::default()
        });
        let room = RoomExternalId::new("v1-acceptance").unwrap();
        let mut participant = Participant::new(
            ParticipantConfig {
                manual_sub: false,
                room_id: entity::RoomId::from_external(&room),
                participant_id: entity::ParticipantId::new(),
                participant_external_id: ParticipantExternalId::new("alice").unwrap(),
                connection_id: entity::ConnectionId::new(),
                profile: ConnectionProfile::Native,
                initial_authorization_expiry: expiry,
                rtc,
                resources,
            },
            ShardId::new(0),
            1200,
            1200,
        );
        for resource in negotiated {
            match resource.direction {
                Direction::RecvOnly => {
                    let kind = match resource.kind {
                        MediaKind::Audio => TrackKind::Audio,
                        MediaKind::Video => TrackKind::Video,
                    };
                    participant.add_v1_test_sender(resource.media_index, kind, resource.mid);
                }
                Direction::SendOnly => participant.add_v1_test_receiver(SlotConfig {
                    media_index: resource.media_index,
                    mid: resource.mid,
                    kind: resource.kind,
                    ssrc: (resource.media_index + 1).into(),
                    ..Default::default()
                }),
                _ => unreachable!(),
            }
        }
        participant.set_v1_test_write_channel_result(true);
        let mut sink = MockParticipantSink::new();
        participant.set_v1_test_channel_config(
            cid,
            ChannelConfig {
                label: "v1/sys/signaling".to_owned(),
                ..Default::default()
            },
        );
        participant.handle_v1_test_event(
            Event::ChannelOpen(cid, "v1/sys/signaling".to_owned()),
            &mut sink,
        );
        participant.stage_v1_test_output().unwrap();
        let mut this = Self {
            participant,
            cid,
            sink,
            handles: SlotMap::with_key(),
            video_receiver_mid,
            video_sender,
            audio_sender,
            recorded: Vec::new(),
        };
        this.drain(false);
        this
    }

    fn messages(&self, from: usize) -> Vec<v1::ServerMessage> {
        self.recorded[from..]
            .iter()
            .map(|b| pulsebeam_proto::codec::decode_server(b).unwrap())
            .collect()
    }

    fn drain(&mut self, stage: bool) {
        if stage {
            self.participant.stage_v1_test_output().unwrap();
        }
        while let Some(bytes) = self.participant.take_v1_test_output() {
            self.recorded.push(bytes);
        }
    }

    fn send(&mut self, payload: v1::client_message::Payload) -> usize {
        let from = self.recorded.len();
        let data = pulsebeam_proto::codec::encode_client(&v1::ClientMessage {
            payload: Some(payload),
        })
        .unwrap();
        self.participant.handle_v1_test_event(
            Event::ChannelData(ChannelData {
                id: self.cid,
                binary: true,
                data,
            }),
            &mut self.sink,
        );
        self.drain(false);
        from
    }

    fn intent(&mut self, intent: v1::Intent) -> usize {
        self.send(v1::client_message::Payload::Intent(intent))
    }

    fn add_track(
        &mut self,
        owner: entity::ParticipantId,
        external: &str,
        kind: TrackKind,
        label: &str,
    ) -> (TrackHandle, entity::TrackId) {
        self.participant.apply(
            ParticipantEffect::ParticipantsChanged {
                added: vec![RoomParticipant {
                    id: owner,
                    external_id: ParticipantExternalId::new(external).unwrap(),
                }],
                removed: vec![],
            },
            None,
        );
        let meta = TrackMeta::labeled_media(
            self.participant.room_id,
            self.participant.shard_id,
            owner,
            kind,
            label.to_owned(),
        );
        let track = match kind {
            TrackKind::Audio => crate::track::new_audio(Mid::from(label), meta).1,
            TrackKind::Video => crate::track::new_video(Mid::from(label), meta, vec![]).1,
            TrackKind::Data => unreachable!(),
        };
        let id = track.id();
        let handle = self.handles.insert(());
        self.participant.apply(
            ParticipantEffect::TrackCandidateAdded { track },
            Some(handle),
        );
        (handle, id)
    }
}

fn empty(revision: u64) -> v1::Intent {
    v1::Intent {
        revision,
        send: None,
        receive: None,
    }
}
fn payload(message: &v1::ServerMessage) -> &v1::server_message::Payload {
    message.payload.as_ref().unwrap()
}

#[test]
fn native_reliable_ordered_channel_open_installs_v1_signaling() {
    let room = RoomExternalId::new("v1-channel-open").unwrap();
    let mut rtc = Rtc::new(std::time::Instant::now());
    let cid = rtc.direct_api().create_data_channel(ChannelConfig {
        label: "v1/sys/signaling".to_owned(),
        ..Default::default()
    });
    let mut participant = Participant::new(
        ParticipantConfig {
            manual_sub: false,
            room_id: entity::RoomId::from_external(&room),
            participant_id: entity::ParticipantId::new(),
            participant_external_id: ParticipantExternalId::new("alice").unwrap(),
            connection_id: entity::ConnectionId::new(),
            profile: ConnectionProfile::Native,
            initial_authorization_expiry: Some(4242),
            rtc,
            resources: NegotiatedResources::empty_for_test(),
        },
        ShardId::new(0),
        1200,
        1200,
    );
    participant.set_v1_test_write_channel_result(true);
    let mut sink = MockParticipantSink::new();
    participant.set_v1_test_channel_config(
        cid,
        ChannelConfig {
            label: "v1/sys/signaling".to_owned(),
            ..Default::default()
        },
    );

    participant.handle_v1_test_event(
        Event::ChannelOpen(cid, "v1/sys/signaling".to_owned()),
        &mut sink,
    );
    participant.stage_v1_test_output().unwrap();
    let messages: Vec<_> = std::iter::from_fn(|| participant.take_v1_test_output())
        .map(|bytes| pulsebeam_proto::codec::decode_server(&bytes).unwrap())
        .collect();

    assert!(matches!(
        payload(&messages[0]),
        v1::server_message::Payload::Authorization(v1::Authorization {
            expires_at_unix_seconds: 4242
        })
    ));
    assert!(matches!(
        payload(&messages[1]),
        v1::server_message::Payload::Catalog(v1::Catalog {
            state: Some(v1::catalog::State::Snapshot(_)),
            ..
        })
    ));
    assert!(matches!(
        payload(&messages[2]),
        v1::server_message::Payload::Mapping(_)
    ));
}

#[test]
fn native_admission_orders_authorization_snapshot_and_mapping_and_retries_exact_bytes() {
    let mut h = Harness::new(Some(4242));
    let messages = h.messages(0);
    let authorization = messages
        .iter()
        .position(|m| {
            matches!(
                payload(m),
                v1::server_message::Payload::Authorization(v1::Authorization {
                    expires_at_unix_seconds: 4242
                })
            )
        })
        .unwrap();
    let catalog = messages
        .iter()
        .position(|m| {
            matches!(
                payload(m),
                v1::server_message::Payload::Catalog(v1::Catalog {
                    state: Some(v1::catalog::State::Snapshot(_)),
                    ..
                })
            )
        })
        .unwrap();
    let mapping = messages
        .iter()
        .position(|m| matches!(payload(m), v1::server_message::Payload::Mapping(_)))
        .unwrap();
    assert!(authorization < catalog && catalog < mapping);
    h.participant.set_v1_test_write_channel_result(false);
    let from = h.participant.v1_test_channel_write_attempts().len();
    assert!(h.participant.stage_v1_test_authorization(7777));
    let _ = h.participant.poll(Instant::now(), &mut h.sink);
    let failed = h.participant.v1_test_channel_write_attempts()[from..].to_vec();
    assert!(!failed.is_empty() && failed.iter().all(|b| b == &failed[0]));
    h.participant.set_v1_test_write_channel_result(true);
    let _ = h.participant.poll(Instant::now(), &mut h.sink);
    assert_eq!(
        h.participant.v1_test_channel_write_attempts().last(),
        failed.first()
    );
}

#[test]
fn room_changes_obey_catalog_mapping_causality_and_option_defaults() {
    let mut h = Harness::new(None);
    let owner = entity::ParticipantId::new();
    let (handle, video) = h.add_track(owner, "bob", TrackKind::Video, "camera");
    let (_, audio) = h.add_track(owner, "bob", TrackKind::Audio, "mic");
    let from = h.intent(v1::Intent {
        revision: 1,
        send: None,
        receive: Some(v1::ReceiveIntent {
            video: Some(v1::VideoIntent {
                tracks: vec![v1::VideoTrackIntent {
                    track_id: video.as_str(),
                    options: Some(v1::VideoOptions {
                        height: 360,
                        min_height: 720,
                        min_fps: 0,
                        priority: 0,
                        playout_delay: Some(v1::PlayoutDelay {
                            min_ms: 3000,
                            max_ms: 500,
                        }),
                    }),
                }],
            }),
            audio: None,
        }),
    });
    let messages = h.messages(from);
    let cat = messages
        .iter()
        .position(|m| matches!(payload(m), v1::server_message::Payload::Catalog(_)))
        .unwrap_or_else(|| panic!("missing catalog: {messages:?}"));
    let map = messages
        .iter()
        .position(|m| {
            matches!(
                payload(m),
                v1::server_message::Payload::Mapping(v1::Mapping {
                    intent_revision: 1,
                    ..
                })
            )
        })
        .unwrap_or_else(|| panic!("missing mapping: {messages:?}"));
    assert!(cat < map);
    let accepted = h.participant.v1_test_intent().unwrap();
    assert_eq!(
        (
            accepted.video[0].target_height,
            accepted.video[0].min_height
        ),
        (360, 720)
    );
    assert_eq!(accepted.video[0].playout, PlayoutPolicy::fixed((3000, 500)));
    assert!(accepted.audio_auto);
    h.participant.v1_test_forward_audio(entity::AudioOrigin {
        participant: owner,
        track: audio,
    });
    assert_eq!(
        h.participant.v1_test_mapping().audio.unwrap().tracks[0].track_id,
        audio.as_str()
    );
    let mapping = h.participant.v1_test_mapping();
    let from = h.recorded.len();
    h.participant.apply(
        ParticipantEffect::TrackCandidateRemoved { track_id: video },
        Some(handle),
    );
    h.drain(true);
    let messages = h.messages(from);
    assert!(
        matches!(payload(&messages[0]), v1::server_message::Payload::Mapping(v1::Mapping { video: Some(v), .. }) if v.tracks.is_empty())
    );
    assert!(matches!(
        payload(&messages[1]),
        v1::server_message::Payload::Catalog(_)
    ));
    assert_eq!(mapping.intent_revision, 1);
    let from = h.intent(empty(1));
    assert!(h.messages(from).iter().any(|m| matches!(
        payload(m),
        v1::server_message::Payload::Mapping(v1::Mapping {
            intent_revision: 1,
            ..
        })
    )));
}

#[test]
fn zero_and_stale_intents_acknowledge_without_applying_conflicting_bodies() {
    let mut h = Harness::new(None);
    h.intent(v1::Intent {
        revision: 1,
        send: Some(v1::SendIntent {
            tracks: vec![v1::LocalTrack {
                sender_index: h.video_sender,
                kind: v1::TrackKind::Video.into(),
                label: "camera".into(),
            }],
        }),
        receive: None,
    });
    let publications = h.participant.v1_test_publications();
    for revision in [0, 1] {
        let from = h.intent(v1::Intent {
            revision,
            send: Some(v1::SendIntent {
                tracks: vec![v1::LocalTrack {
                    sender_index: h.video_sender,
                    kind: v1::TrackKind::Video.into(),
                    label: "conflicting-label".into(),
                }],
            }),
            receive: None,
        });
        assert_eq!(h.participant.v1_test_publications(), publications);
        assert!(h.messages(from).iter().any(|message| matches!(
            payload(message),
            v1::server_message::Payload::Mapping(v1::Mapping {
                intent_revision: 1,
                ..
            })
        )));
        assert!(
            !h.messages(from)
                .iter()
                .any(|message| matches!(payload(message), v1::server_message::Payload::Error(_)))
        );
    }
}

#[test]
fn fatal_relabel_and_capacity_are_revisioned_and_atomic() {
    let mut h = Harness::new(None);
    h.intent(v1::Intent {
        revision: 1,
        send: Some(v1::SendIntent {
            tracks: vec![v1::LocalTrack {
                sender_index: h.video_sender,
                kind: v1::TrackKind::Video.into(),
                label: "camera".into(),
            }],
        }),
        receive: None,
    });
    let before = h.participant.v1_test_publications();
    let from = h.intent(v1::Intent {
        revision: 2,
        send: Some(v1::SendIntent {
            tracks: vec![v1::LocalTrack {
                sender_index: h.video_sender,
                kind: v1::TrackKind::Video.into(),
                label: "screen".into(),
            }],
        }),
        receive: None,
    });
    assert_eq!(h.participant.v1_test_publications(), before);
    assert!(
        matches!(payload(h.messages(from).last().unwrap()), v1::server_message::Payload::Error(v1::Error { code, fatal: true, intent_revision: Some(2), .. }) if *code == v1::ErrorCode::ProtocolError as i32)
    );

    let mut h = Harness::new(None);
    let owner = entity::ParticipantId::new();
    let (_, one) = h.add_track(owner, "bob", TrackKind::Video, "one");
    let (_, two) = h.add_track(owner, "bob", TrackKind::Video, "two");
    h.intent(v1::Intent {
        revision: 1,
        send: Some(v1::SendIntent {
            tracks: vec![v1::LocalTrack {
                sender_index: h.audio_sender,
                kind: v1::TrackKind::Audio.into(),
                label: "mic".into(),
            }],
        }),
        receive: None,
    });
    let before = h.participant.v1_test_publications();
    let tracks = [one, two]
        .into_iter()
        .map(|id| v1::VideoTrackIntent {
            track_id: id.as_str(),
            options: None,
        })
        .collect();
    let from = h.intent(v1::Intent {
        revision: 2,
        send: None,
        receive: Some(v1::ReceiveIntent {
            video: Some(v1::VideoIntent { tracks }),
            audio: None,
        }),
    });
    assert_eq!(h.participant.v1_test_publications(), before);
    assert!(matches!(
        payload(h.messages(from).last().unwrap()),
        v1::server_message::Payload::Error(v1::Error {
            intent_revision: Some(2),
            fatal: true,
            ..
        })
    ));
}

#[test]
fn malformed_wire_matrix_is_terminal_without_mutation() {
    let mut overflow = vec![0x1f, b'a', 1, 0];
    overflow.extend(std::iter::repeat_n(255, 128));
    overflow.push(109);
    for (binary, data) in [
        (false, vec![0]),
        (true, vec![0]),
        (true, vec![0xff]),
        (true, vec![0; 32913]),
        (true, overflow),
    ] {
        let mut h = Harness::new(None);
        let from = h.recorded.len();
        h.participant.handle_v1_test_event(
            Event::ChannelData(ChannelData {
                id: h.cid,
                binary,
                data,
            }),
            &mut h.sink,
        );
        h.drain(false);
        assert_eq!(h.participant.v1_test_mapping().intent_revision, 0);
        assert!(
            matches!(payload(h.messages(from).last().unwrap()), v1::server_message::Payload::Error(v1::Error { code, fatal: true, .. }) if *code == v1::ErrorCode::InvalidMessage as i32)
        );
    }
}

#[test]
fn renewal_expiry_reconnect_and_retired_input_are_fenced() {
    let mut h = Harness::new(None);
    h.send(v1::client_message::Payload::RenewAuthorization(
        v1::RenewAuthorization { token: "ok".into() },
    ));
    let request = h
        .participant
        .v1_test_pending_authorization_request()
        .unwrap();
    let from = h.recorded.len();
    h.participant.apply(
        ParticipantEffect::AuthorizationRenewed {
            participant_id: h.participant.participant_id,
            connection_id: h.participant.connection_id,
            request_id: request,
            expires_at_unix_seconds: 9999,
        },
        None,
    );
    h.drain(false);
    assert!(matches!(
        payload(&h.messages(from)[0]),
        v1::server_message::Payload::Authorization(_)
    ));
    h.send(v1::client_message::Payload::RenewAuthorization(
        v1::RenewAuthorization { token: "no".into() },
    ));
    let request = h
        .participant
        .v1_test_pending_authorization_request()
        .unwrap();
    let from = h.recorded.len();
    h.participant.apply(
        ParticipantEffect::AuthorizationRejected {
            participant_id: h.participant.participant_id,
            connection_id: h.participant.connection_id,
            request_id: request,
        },
        None,
    );
    h.drain(false);
    assert!(matches!(
        payload(&h.messages(from)[0]),
        v1::server_message::Payload::Error(v1::Error { fatal: false, .. })
    ));

    let mut expired = Harness::new(None);
    let from = expired.recorded.len();
    expired.participant.apply(
        ParticipantEffect::AuthorizationExpired {
            connection_id: expired.participant.connection_id,
        },
        None,
    );
    expired.drain(true);
    let messages = expired.messages(from);
    assert!(matches!(
        payload(&messages[0]),
        v1::server_message::Payload::Error(v1::Error { code, fatal: true, .. })
            if *code == v1::ErrorCode::AuthorizationExpired as i32
    ));
    let expired_revision = expired.participant.v1_test_mapping().intent_revision;
    expired.send(v1::client_message::Payload::Intent(empty(9)));
    assert_eq!(
        expired.participant.v1_test_mapping().intent_revision,
        expired_revision
    );

    let mut replacement = Harness::new(None);
    replacement.participant.apply(
        ParticipantEffect::AuthorizationExpired {
            connection_id: entity::ConnectionId::new(),
        },
        None,
    );
    replacement.intent(empty(1));
    assert_eq!(replacement.participant.v1_test_mapping().intent_revision, 1);

    let mut reconnect = Harness::new(None);
    let owner = entity::ParticipantId::new();
    let (_, video) = reconnect.add_track(owner, "bob", TrackKind::Video, "camera");
    let receive = |revision, options| v1::Intent {
        revision,
        send: None,
        receive: Some(v1::ReceiveIntent {
            video: Some(v1::VideoIntent {
                tracks: vec![v1::VideoTrackIntent {
                    track_id: video.as_str(),
                    options,
                }],
            }),
            audio: None,
        }),
    };
    reconnect.intent(receive(
        1,
        Some(v1::VideoOptions {
            playout_delay: Some(v1::PlayoutDelay {
                min_ms: 10,
                max_ms: 10,
            }),
            ..Default::default()
        }),
    ));
    reconnect
        .participant
        .lock_v1_test_playout(MediaKind::Video, reconnect.video_receiver_mid);
    let from = reconnect.intent(receive(2, None));
    assert!(matches!(
        payload(reconnect.messages(from).last().unwrap()),
        v1::server_message::Payload::Reconnect(_)
    ));
    reconnect.participant.apply(
        ParticipantEffect::AuthorizationExpired {
            connection_id: reconnect.participant.connection_id,
        },
        None,
    );
    reconnect.participant.apply(
        ParticipantEffect::ParticipantsChanged {
            added: vec![],
            removed: vec![owner],
        },
        None,
    );
    reconnect.participant.handle_v1_test_event(
        Event::ChannelData(ChannelData {
            id: reconnect.cid,
            binary: true,
            data: pulsebeam_proto::codec::encode_client(&v1::ClientMessage {
                payload: Some(v1::client_message::Payload::Intent(empty(9))),
            })
            .unwrap(),
        }),
        &mut reconnect.sink,
    );
    assert_eq!(reconnect.participant.v1_test_mapping().intent_revision, 1);
}
