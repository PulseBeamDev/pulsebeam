use pulsebeam_proto::prelude::Message;
use pulsebeam_proto::reliable::RelDelivery;
use std::collections::VecDeque;
use str0m::channel::ChannelId;
use str0m::media::KeyframeRequestKind;

use super::worker::ShardEvent;
use crate::entity::{ParticipantId, RoomId, TrackId, TrackKind};
use crate::keys::{DownstreamSlotKey, ParticipantKey};
use crate::participant::event::ParticipantSink;
use crate::rtp::RtpPacket;
use crate::shard::router::{DataStreamKey, ReliableStreamKey, TrackKey};
use crate::track::{GlobalKeyframeRequest, StreamId, Topic, Track, TrackLayer, TrackMeta};

pub struct AudioRtpEvent {
    pub stream_id: StreamId,
    pub pkt: RtpPacket,
    /// The semantic origin, which subscribers need in order to attribute the
    /// audio. Read once per delivery, never hashed.
    pub origin: ParticipantId,
    /// The origin's compiled key, present only for a locally published
    /// stream. It exists so the fanout can skip the publisher without
    /// hashing its name back to a key; a remote origin has no key here and
    /// no member of this room to skip.
    pub origin_key: Option<ParticipantKey>,
    /// The publisher's compiled fanout — see [`VideoRtpEvent::fanout`].
    pub fanout: Option<TrackKey>,
}

pub struct VideoRtpEvent {
    pub stream_id: StreamId,
    pub pkt: RtpPacket,
    /// The publisher's compiled fanout, resolved once per SSRC rather than
    /// hashed per packet. `None` only until the shard binds one.
    pub fanout: Option<TrackKey>,
}

pub struct SctpEvent<K> {
    pub pkt: Vec<u8>,
    pub stream: Option<K>,
}

/// The compiled identity of the participant emitting into the pipeline.
///
/// A sink is built per participant in the dirty loop, which already holds
/// both the key and the name, so carrying both costs nothing and lets the
/// hot events use keys while the lifecycle events keep using names.
#[derive(Clone, Copy)]
pub(crate) struct SinkIdentity {
    pub id: ParticipantId,
    pub key: ParticipantKey,
    pub room_id: RoomId,
}

pub enum ParticipantEvent {
    Subscription(ParticipantSubscriptionEvent),
    Lifecycle(ParticipantLifecycleEvent),
    Control(ClientIntent),
}

pub enum ParticipantSubscriptionEvent {
    Subscribed {
        subscriber: ParticipantId,
        subscriber_key: ParticipantKey,
        slot: DownstreamSlotKey,
        track: TrackMeta,
    },
    Unsubscribed {
        subscriber: ParticipantId,
        slot: DownstreamSlotKey,
        track: TrackMeta,
    },
}

pub enum ParticipantLifecycleEvent {
    Exited {
        participant_id: ParticipantId,
        participant_key: ParticipantKey,
    },
}

#[derive(Debug)]
pub enum ClientIntent {
    TrackSubscribed {
        subscriber: ParticipantId,
        subscriber_key: ParticipantKey,
        slot: DownstreamSlotKey,
        track: TrackMeta,
    },
    TrackUnsubscribed {
        subscriber: ParticipantId,
        slot: DownstreamSlotKey,
        track: TrackMeta,
    },
    TrackPublished(Track, crate::track::TrackStates),
    /// A published track's latest measurements, refreshed on the slow poll.
    ///
    /// Within-shard: the participant that measures hands its own shard core a
    /// value, which forwards it to the destinations holding a route. Nothing
    /// reaches into a monitor from outside.
    TrackStatsUpdated {
        track_id: TrackId,
        states: crate::track::TrackStates,
    },
    TrackUnpublished {
        origin: ParticipantId,
        track_id: TrackId,
    },
    DataTopicPublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    DataTopicUnpublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    DataTopicSubscribed {
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: Topic,
        publisher: Option<ParticipantId>,
        channel: ChannelId,
    },
    DataTopicUnsubscribed {
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: Topic,
        publisher: Option<ParticipantId>,
    },
    KeyframeRequested(GlobalKeyframeRequest),
    ReliableDataTopicPublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    ReliableDataTopicUnpublished {
        room_id: RoomId,
        publisher: ParticipantId,
        topic: Topic,
    },
    ReliableDataTopicSubscribed {
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: Topic,
        channel: ChannelId,
    },
    ReliableDataTopicUnsubscribed {
        room_id: RoomId,
        subscriber: ParticipantId,
        topic: Topic,
    },
    ReliableControlReceived {
        stream: Option<ReliableStreamKey>,
        bytes: Vec<u8>,
    },
}

pub(crate) struct EventPipeline {
    participant_events: VecDeque<ParticipantEvent>,
    audio_queue: VecDeque<AudioRtpEvent>,
    video_queue: VecDeque<VideoRtpEvent>,
    data_queue: VecDeque<SctpEvent<DataStreamKey>>,
    reliable_data_queue: VecDeque<SctpEvent<ReliableStreamKey>>,
    shard_events: VecDeque<ShardEvent>,
}

impl EventPipeline {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            participant_events: VecDeque::with_capacity(cap),
            audio_queue: VecDeque::with_capacity(cap),
            video_queue: VecDeque::with_capacity(cap),
            data_queue: VecDeque::with_capacity(cap),
            reliable_data_queue: VecDeque::with_capacity(cap),
            shard_events: VecDeque::with_capacity(cap),
        }
    }

    pub fn participant_sink(&mut self, who: SinkIdentity) -> PipelineSinkRef<'_> {
        PipelineSinkRef {
            id: who.id,
            key: who.key,
            room_id: who.room_id,
            pipeline: self,
        }
    }

    pub fn pop_participant_event(&mut self) -> Option<ParticipantEvent> {
        self.participant_events.pop_front()
    }

    pub fn pop_audio_rtp(&mut self) -> Option<AudioRtpEvent> {
        self.audio_queue.pop_front()
    }

    pub fn pop_video_rtp(&mut self) -> Option<VideoRtpEvent> {
        self.video_queue.pop_front()
    }

    pub fn push_shard_event(&mut self, ev: ShardEvent) {
        self.shard_events.push_back(ev);
    }

    pub fn pop_data_sctp(&mut self) -> Option<SctpEvent<DataStreamKey>> {
        self.data_queue.pop_front()
    }

    pub fn pop_reliable_data_sctp(&mut self) -> Option<SctpEvent<ReliableStreamKey>> {
        self.reliable_data_queue.pop_front()
    }

    pub fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.shard_events.pop_front()
    }
}

pub struct PipelineSinkRef<'a> {
    id: ParticipantId,
    key: ParticipantKey,
    room_id: RoomId,
    pipeline: &'a mut EventPipeline,
}

impl<'a> ParticipantSink for PipelineSinkRef<'a> {
    #[inline]
    fn subscribe(&mut self, track: TrackMeta, slot: DownstreamSlotKey) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Subscription(
                ParticipantSubscriptionEvent::Subscribed {
                    subscriber: self.id,
                    subscriber_key: self.key,
                    slot,
                    track,
                },
            ));
    }

    #[inline]
    fn unsubscribe(&mut self, track: TrackMeta, slot: DownstreamSlotKey) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Subscription(
                ParticipantSubscriptionEvent::Unsubscribed {
                    subscriber: self.id,
                    slot,
                    track,
                },
            ));
    }

    #[inline]
    fn publish_track_stats(&mut self, track_id: TrackId, states: crate::track::TrackStates) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ClientIntent::TrackStatsUpdated {
                track_id,
                states,
            }));
    }

    fn publish_track(&mut self, track: Track, states: crate::track::TrackStates) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ClientIntent::TrackPublished(
                track, states,
            )));
    }

    #[inline]
    fn unpublish_track(&mut self, track_id: TrackId) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ClientIntent::TrackUnpublished {
                origin: self.id,
                track_id,
            }));
    }

    #[inline]
    fn subscribe_data_topic(
        &mut self,
        topic: Topic,
        publisher: Option<ParticipantId>,
        channel: ChannelId,
    ) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::DataTopicSubscribed {
                    room_id: self.room_id,
                    subscriber: self.id,
                    topic,
                    publisher,
                    channel,
                },
            ));
    }

    #[inline]
    fn unsubscribe_data_topic(
        &mut self,
        topic: Topic,
        publisher: Option<ParticipantId>,
        _channel: ChannelId,
    ) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::DataTopicUnsubscribed {
                    room_id: self.room_id,
                    subscriber: self.id,
                    topic,
                    publisher,
                },
            ));
    }

    #[inline]
    fn publish_data_topic(&mut self, topic: Topic) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::DataTopicPublished {
                    room_id: self.room_id,
                    publisher: self.id,
                    topic,
                },
            ));
    }

    #[inline]
    fn unpublish_data_topic(&mut self, topic: Topic) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::DataTopicUnpublished {
                    room_id: self.room_id,
                    publisher: self.id,
                    topic,
                },
            ));
    }

    #[inline]
    fn request_keyframe(&mut self, layer: &TrackLayer) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ClientIntent::KeyframeRequested(
                GlobalKeyframeRequest {
                    shard_id: layer.meta.shard_id,
                    origin: layer.meta.origin,
                    stream_id: layer.stream_id(),
                    kind: KeyframeRequestKind::Pli,
                },
            )));
    }

    #[inline]
    fn exit(&mut self) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Lifecycle(
                ParticipantLifecycleEvent::Exited {
                    participant_id: self.id,
                    participant_key: self.key,
                },
            ));
    }

    #[inline]
    fn publish_rtp(&mut self, stream_id: StreamId, fanout: Option<TrackKey>, pkt: RtpPacket) {
        match stream_id.0.kind() {
            TrackKind::Audio => self.pipeline.audio_queue.push_back(AudioRtpEvent {
                stream_id,
                pkt,
                origin: self.id,
                origin_key: Some(self.key),
                fanout,
            }),
            TrackKind::Video => self.pipeline.video_queue.push_back(VideoRtpEvent {
                stream_id,
                pkt,
                fanout,
            }),
            TrackKind::Data => {}
        }
    }

    #[inline]
    fn publish_sctp(&mut self, _topic: Topic, stream: Option<DataStreamKey>, pkt: Vec<u8>) {
        self.pipeline
            .data_queue
            .push_back(SctpEvent { pkt, stream });
    }

    #[inline]
    fn publish_reliable_data_topic(&mut self, topic: Topic) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::ReliableDataTopicPublished {
                    room_id: self.room_id,
                    publisher: self.id,
                    topic,
                },
            ));
    }

    #[inline]
    fn unpublish_reliable_data_topic(&mut self, topic: Topic) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::ReliableDataTopicUnpublished {
                    room_id: self.room_id,
                    publisher: self.id,
                    topic,
                },
            ));
    }

    #[inline]
    fn subscribe_reliable_data_topic(&mut self, topic: Topic, channel: ChannelId) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::ReliableDataTopicSubscribed {
                    room_id: self.room_id,
                    subscriber: self.id,
                    topic,
                    channel,
                },
            ));
    }

    #[inline]
    fn unsubscribe_reliable_data_topic(&mut self, topic: Topic, _channel: ChannelId) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::ReliableDataTopicUnsubscribed {
                    room_id: self.room_id,
                    subscriber: self.id,
                    topic,
                },
            ));
    }

    #[inline]
    fn publish_reliable_sctp(
        &mut self,
        _topic: Topic,
        stream: Option<ReliableStreamKey>,
        frame: Vec<u8>,
    ) {
        let frame = RelDelivery {
            publisher_id: self.id.as_str(),
            frame,
        }
        .encode_to_vec();
        self.pipeline
            .reliable_data_queue
            .push_back(SctpEvent { pkt: frame, stream });
    }

    #[inline]
    fn forward_reliable_control(
        &mut self,
        _publisher: ParticipantId,
        _topic: Topic,
        stream: Option<ReliableStreamKey>,
        bytes: Vec<u8>,
    ) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ClientIntent::ReliableControlReceived { stream, bytes },
            ));
    }
}
