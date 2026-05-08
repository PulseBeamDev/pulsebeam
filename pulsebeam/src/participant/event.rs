use std::collections::VecDeque;

use str0m::media::KeyframeRequestKind;
use tokio::time::Instant;

use crate::entity::{ParticipantId, RoomId};
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
    KeyframeRequested(GlobalKeyframeRequest),
}

pub struct EventQueue<'a> {
    id: &'a ParticipantId,
    room_id: RoomId,
    queue: &'a mut VecDeque<ParticipantEvent>,
    rtp_queue: &'a mut VecDeque<RtpEvent>,
}

impl<'a> EventQueue<'a> {
    pub fn new(
        id: &'a ParticipantId,
        room_id: RoomId,
        queue: &'a mut VecDeque<ParticipantEvent>,
        rtp_queue: &'a mut VecDeque<RtpEvent>,
    ) -> Self {
        Self {
            id,
            room_id,
            queue,
            rtp_queue,
        }
    }

    pub fn subscribe(&mut self, track: TrackMeta) {
        self.queue.push_back(ParticipantEvent::Topology(
            ParticipantTopologyEvent::TrackSubscribed {
                subscriber: *self.id,
                track,
            },
        ));
    }

    pub fn unsubscribe(&mut self, track: TrackMeta) {
        self.queue.push_back(ParticipantEvent::Topology(
            ParticipantTopologyEvent::TrackUnsubscribed {
                subscriber: *self.id,
                track,
            },
        ));
    }

    pub fn publish_rtp(&mut self, stream_id: StreamId, pkt: RtpPacket) {
        self.rtp_queue.push_back(RtpEvent {
            stream_id,
            pkt,
            room_id: self.room_id,
            origin: *self.id,
        });
    }

    pub fn publish_track(&mut self, track: Track) {
        self.queue.push_back(ParticipantEvent::Control(
            ParticipantControlEvent::TrackPublished(track),
        ));
    }

    pub fn request_keyframe(&mut self, layer: &TrackLayer) {
        self.queue.push_back(ParticipantEvent::Control(
            ParticipantControlEvent::KeyframeRequested(GlobalKeyframeRequest {
                shard_id: layer.meta.shard_id,
                origin: layer.meta.origin,
                stream_id: layer.stream_id(),
                kind: KeyframeRequestKind::Pli,
            }),
        ));
    }

    pub fn update_deadline(&mut self, deadline: Instant) {
        self.queue.push_back(ParticipantEvent::Timer(
            ParticipantTimerEvent::DeadlineUpdated {
                at: deadline,
                participant_id: *self.id,
            },
        ));
    }

    pub fn exit(&mut self) {
        self.queue.push_back(ParticipantEvent::Lifecycle(
            ParticipantLifecycleEvent::Exited {
                participant_id: *self.id,
            },
        ));
    }
}
