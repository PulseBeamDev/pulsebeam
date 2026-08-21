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
use crate::shard::router::{ReliableStreamKey, TrackKey, UnreliableStreamKey};
use crate::track::{GlobalKeyframeRequest, StreamId, Topic, Track, TrackLayer, TrackMeta};

pub struct AudioRtpEvent {
    pub stream_id: StreamId,
    pub pkt: RtpPacket,
    /// The semantic origin, which subscribers need in order to attribute the
    /// audio. Read once per delivery, never hashed.
    pub origin: ParticipantId,
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
    Control(ShardEvent),
    Internal(ShardInternalEvent),
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
    Connected {
        participant_key: ParticipantKey,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        source_shard: crate::id::ShardId,
    },
    Exited {
        participant_id: ParticipantId,
        participant_key: ParticipantKey,
    },
}

pub enum ShardInternalEvent {
    TrackStatsUpdated {
        track_id: TrackId,
        fanout: Option<TrackKey>,
        states: crate::track::TrackStates,
    },
    KeyframeRequested {
        request: GlobalKeyframeRequest,
        fanout: Option<TrackKey>,
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
    data_queue: VecDeque<SctpEvent<UnreliableStreamKey>>,
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

    pub fn pop_data_sctp(&mut self) -> Option<SctpEvent<UnreliableStreamKey>> {
        self.data_queue.pop_front()
    }

    pub fn pop_reliable_data_sctp(&mut self) -> Option<SctpEvent<ReliableStreamKey>> {
        self.reliable_data_queue.pop_front()
    }

    pub fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.shard_events.pop_front()
    }

    pub fn has_pending(&self) -> bool {
        !self.participant_events.is_empty()
            || !self.audio_queue.is_empty()
            || !self.video_queue.is_empty()
            || !self.data_queue.is_empty()
            || !self.reliable_data_queue.is_empty()
            || !self.shard_events.is_empty()
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
    fn connected(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        source_shard: crate::id::ShardId,
    ) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Lifecycle(
                ParticipantLifecycleEvent::Connected {
                    participant_key: self.key,
                    source,
                    destination,
                    source_shard,
                },
            ));
    }

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
    fn publish_track_stats(
        &mut self,
        track_id: TrackId,
        fanout: Option<TrackKey>,
        states: crate::track::TrackStates,
    ) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::TrackStatsUpdated {
                    track_id,
                    fanout,
                    states,
                },
            ));
    }

    fn publish_track(&mut self, track: Track, states: crate::track::TrackStates) {
        let mut track = track;
        track.reverse = None;
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ShardEvent::TrackPublished {
                track: Box::new(track),
                states,
            }));
    }

    #[inline]
    fn unpublish_track(&mut self, track_id: TrackId) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ShardEvent::TrackUnpublished {
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
            .push_back(ParticipantEvent::Control(ShardEvent::DataTopicSubscribed {
                room_id: self.room_id,
                subscriber: self.id,
                topic,
                publisher,
                channel,
            }));
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
                ShardEvent::DataTopicUnsubscribed {
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
            .push_back(ParticipantEvent::Control(ShardEvent::DataTopicPublished {
                room_id: self.room_id,
                publisher: self.id,
                publisher_key: self.key,
                topic,
            }));
    }

    #[inline]
    fn unpublish_data_topic(&mut self, topic: Topic) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::DataTopicUnpublished {
                    room_id: self.room_id,
                    publisher: self.id,
                    publisher_key: self.key,
                    topic,
                },
            ));
    }

    #[inline]
    fn request_keyframe(&mut self, layer: &TrackLayer, fanout: Option<TrackKey>) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::KeyframeRequested {
                    request: GlobalKeyframeRequest {
                        shard_id: layer.meta.shard_id,
                        origin: layer.meta.origin,
                        stream_id: layer.stream_id(),
                        kind: KeyframeRequestKind::Pli,
                    },
                    fanout,
                },
            ));
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
    fn publish_sctp(&mut self, _topic: Topic, stream: Option<UnreliableStreamKey>, pkt: Vec<u8>) {
        self.pipeline
            .data_queue
            .push_back(SctpEvent { pkt, stream });
    }

    #[inline]
    fn publish_reliable_data_topic(&mut self, topic: Topic) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::ReliableDataTopicPublished {
                    room_id: self.room_id,
                    publisher: self.id,
                    publisher_key: self.key,
                    topic,
                },
            ));
    }

    #[inline]
    fn unpublish_reliable_data_topic(&mut self, topic: Topic) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::ReliableDataTopicUnpublished {
                    room_id: self.room_id,
                    publisher: self.id,
                    publisher_key: self.key,
                    topic,
                },
            ));
    }

    #[inline]
    fn subscribe_reliable_data_topic(&mut self, topic: Topic, channel: ChannelId) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::ReliableDataTopicSubscribed {
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
                ShardEvent::ReliableDataTopicUnsubscribed {
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
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::ReliableControlReceived { stream, bytes },
            ));
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::entity::{ExternalRoomId, TrackKind};

    fn identity() -> SinkIdentity {
        let room = ExternalRoomId::new("room").unwrap();
        SinkIdentity {
            id: ParticipantId::new(),
            key: ParticipantKey::default(),
            room_id: RoomId::from_external(&room),
        }
    }

    fn stream(kind: TrackKind) -> StreamId {
        (ParticipantId::new().derive_track_id(kind, "t"), None)
    }

    /// Each kind of event lands in its own queue, and only its own.
    ///
    /// Everything a participant produces fans out through this one sink, and the shard drains the
    /// queues separately at different rates - audio per packet, lifecycle per tick. A misfiled
    /// event is not dropped, it is delivered to the wrong loop, which is far harder to see than a
    /// loss: the media keeps flowing and the wrong thing acts on it.
    #[test]
    fn every_event_lands_in_its_own_queue() {
        let mut pipeline = EventPipeline::with_capacity(4);
        let who = identity();

        let mut sink = pipeline.participant_sink(who);
        sink.publish_rtp(stream(TrackKind::Audio), None, RtpPacket::default());
        sink.publish_rtp(stream(TrackKind::Video), None, RtpPacket::default());
        sink.publish_sctp(Topic::for_test("t"), None, vec![1]);
        sink.exit();

        assert!(pipeline.pop_audio_rtp().is_some(), "audio went to audio");
        assert!(pipeline.pop_video_rtp().is_some(), "video went to video");
        assert!(pipeline.pop_data_sctp().is_some(), "data went to data");
        assert!(
            pipeline.pop_participant_event().is_some(),
            "lifecycle went to participant events"
        );

        assert!(pipeline.pop_audio_rtp().is_none(), "and nothing crossed");
        assert!(pipeline.pop_video_rtp().is_none());
        assert!(pipeline.pop_data_sctp().is_none());
        assert!(pipeline.pop_reliable_data_sctp().is_none());
    }

    /// Data has no RTP form, so a data-kind stream must not become a media packet.
    #[test]
    fn a_data_track_is_not_forwarded_as_media() {
        let mut pipeline = EventPipeline::with_capacity(2);
        let who = identity();
        pipeline.participant_sink(who).publish_rtp(
            stream(TrackKind::Data),
            None,
            RtpPacket::default(),
        );

        assert!(pipeline.pop_audio_rtp().is_none());
        assert!(pipeline.pop_video_rtp().is_none());
        assert!(!pipeline.has_pending(), "and it queued nothing at all");
    }

    /// Audio carries who spoke; video does not need to.
    ///
    /// Audio slots are shared and stolen, so the subscriber cannot tell from the media who it is
    /// hearing - the origin has to travel with the packet. Video arrives on a slot bound to one
    /// track, so it is already unambiguous.
    #[test]
    fn audio_carries_its_origin() {
        let mut pipeline = EventPipeline::with_capacity(2);
        let who = identity();
        let expected = who.id;
        pipeline.participant_sink(who).publish_rtp(
            stream(TrackKind::Audio),
            None,
            RtpPacket::default(),
        );

        let event = pipeline.pop_audio_rtp().expect("audio was queued");
        assert_eq!(event.origin, expected);
    }

    /// Queues drain in the order they were filled. Media is a sequence, and reordering it here
    /// would be indistinguishable from reordering on the wire.
    #[test]
    fn a_queue_preserves_order() {
        let mut pipeline = EventPipeline::with_capacity(4);
        let who = identity();
        let mut sink = pipeline.participant_sink(who);
        for n in 0..3u8 {
            sink.publish_sctp(Topic::for_test("t"), None, vec![n]);
        }

        let drained: Vec<Vec<u8>> = std::iter::from_fn(|| pipeline.pop_data_sctp())
            .map(|event| event.pkt)
            .collect();
        assert_eq!(drained, vec![vec![0], vec![1], vec![2]]);
    }

    /// `has_pending` decides whether the shard loops again, so it has to agree with every queue.
    /// One it does not check is work the shard parks on until something else happens to wake it.
    #[test]
    fn has_pending_agrees_with_every_queue() {
        let who = identity();

        let mut pipeline = EventPipeline::with_capacity(2);
        assert!(!pipeline.has_pending(), "an empty pipeline has no work");

        pipeline.push_shard_event(ShardEvent::ParticipantClosed {
            participant: who.id,
            key: who.key,
        });
        assert!(pipeline.has_pending(), "a shard event is work");
        assert!(pipeline.pop_shard_event().is_some());
        assert!(!pipeline.has_pending());

        pipeline.participant_sink(who).publish_rtp(
            stream(TrackKind::Video),
            None,
            RtpPacket::default(),
        );
        assert!(pipeline.has_pending(), "so is a video packet");
        assert!(pipeline.pop_video_rtp().is_some());
        assert!(!pipeline.has_pending(), "and draining it clears the flag");
    }
}
