use std::collections::VecDeque;
use str0m::media::KeyframeRequestKind;
use tokio::time::Instant;

use super::worker::ShardEvent;
use crate::entity::{ParticipantId, RoomId, TrackId, TrackKind};
use crate::participant::event::ParticipantSink;
use crate::rtp::RtpPacket;
use crate::track::{GlobalKeyframeRequest, StreamId, Track, TrackLayer, TrackMeta};

pub struct RtpEvent {
    pub stream_id: StreamId,
    pub pkt: RtpPacket,
    pub room_id: RoomId,
    /// The participant that published this packet (used to skip self-forwarding on audio fanout).
    pub origin: ParticipantId,
}

pub enum ParticipantEvent {
    Topology(ParticipantTopologyEvent),
    Timer(ParticipantTimerEvent),
    Lifecycle(ParticipantLifecycleEvent),
    Control(ParticipantControlEvent),
}

pub enum ParticipantTopologyEvent {
    TrackSubscribed {
        subscriber: ParticipantId,
        track: TrackMeta,
    },
    TrackUnsubscribed {
        subscriber: ParticipantId,
        track: TrackMeta,
    },
}

pub enum ParticipantTimerEvent {
    DeadlineUpdated {
        at: Instant,
        participant_id: ParticipantId,
    },
}

pub enum ParticipantLifecycleEvent {
    Exited { participant_id: ParticipantId },
}

pub enum ParticipantControlEvent {
    TrackPublished(Track),
    TrackUnpublished {
        origin: ParticipantId,
        track_id: TrackId,
    },
    KeyframeRequested(GlobalKeyframeRequest),
}

pub(crate) struct EventPipeline {
    participant_events: VecDeque<ParticipantEvent>,
    audio_queue: VecDeque<RtpEvent>,
    video_queue: VecDeque<RtpEvent>,
    data_queue: VecDeque<RtpEvent>, // Added to handle the full match breakdown
    shard_events: VecDeque<ShardEvent>,
}

impl EventPipeline {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            participant_events: VecDeque::with_capacity(cap),
            audio_queue: VecDeque::with_capacity(cap),
            video_queue: VecDeque::with_capacity(cap),
            data_queue: VecDeque::with_capacity(cap),
            shard_events: VecDeque::with_capacity(cap),
        }
    }

    pub fn participant_sink(&mut self, room_id: RoomId, id: ParticipantId) -> PipelineSinkRef<'_> {
        PipelineSinkRef {
            id,
            room_id,
            pipeline: self,
        }
    }

    pub fn pop_participant_event(&mut self) -> Option<ParticipantEvent> {
        self.participant_events.pop_front()
    }

    pub fn pop_audio_rtp(&mut self) -> Option<RtpEvent> {
        self.audio_queue.pop_front()
    }

    pub fn pop_video_rtp(&mut self) -> Option<RtpEvent> {
        self.video_queue.pop_front()
    }

    pub fn push_shard_event(&mut self, ev: ShardEvent) {
        self.shard_events.push_back(ev);
    }

    pub fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.shard_events.pop_front()
    }

    pub fn shard_events_mut(&mut self) -> &mut VecDeque<ShardEvent> {
        &mut self.shard_events
    }
}

pub struct PipelineSinkRef<'a> {
    id: ParticipantId,
    room_id: RoomId,
    pipeline: &'a mut EventPipeline,
}

impl<'a> ParticipantSink for PipelineSinkRef<'a> {
    #[inline]
    fn subscribe(&mut self, track: TrackMeta) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Topology(
                ParticipantTopologyEvent::TrackSubscribed {
                    subscriber: self.id,
                    track,
                },
            ));
    }

    #[inline]
    fn unsubscribe(&mut self, track: TrackMeta) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Topology(
                ParticipantTopologyEvent::TrackUnsubscribed {
                    subscriber: self.id,
                    track,
                },
            ));
    }

    #[inline]
    fn publish_track(&mut self, track: Track) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ParticipantControlEvent::TrackPublished(track),
            ));
    }

    #[inline]
    fn unpublish_track(&mut self, track_id: TrackId) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ParticipantControlEvent::TrackUnpublished {
                    origin: self.id,
                    track_id,
                },
            ));
    }

    #[inline]
    fn request_keyframe(&mut self, layer: &TrackLayer) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ParticipantControlEvent::KeyframeRequested(GlobalKeyframeRequest {
                    shard_id: layer.meta.shard_id,
                    origin: layer.meta.origin,
                    stream_id: layer.stream_id(),
                    kind: KeyframeRequestKind::Pli,
                }),
            ));
    }

    #[inline]
    fn update_deadline(&mut self, deadline: Instant) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Timer(
                ParticipantTimerEvent::DeadlineUpdated {
                    at: deadline,
                    participant_id: self.id,
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
                },
            ));
    }

    #[inline]
    fn publish_rtp(&mut self, stream_id: StreamId, pkt: RtpPacket) {
        let event = RtpEvent {
            stream_id,
            pkt,
            room_id: self.room_id,
            origin: self.id,
        };

        match stream_id.0.kind() {
            TrackKind::Audio => self.pipeline.audio_queue.push_back(event),
            TrackKind::Video => self.pipeline.video_queue.push_back(event),
            TrackKind::Data => todo!("remove data"),
        }
    }
}
