use std::collections::VecDeque;
use str0m::media::KeyframeRequestKind;
use tokio::time::Instant;

use crate::entity::ParticipantId;
use crate::rtp::RtpPacket;
use crate::track::{StreamId, Track, TrackLayer};

pub enum ParticipantEvent {
    PublishedTrack(Track),
    PublishedRtp(StreamId, RtpPacket),
    SubscribedTrack(ParticipantId, StreamId),
    UnsubscribedTrack(ParticipantId, StreamId),
    NewDeadline(Instant, ParticipantId),
    Exited(ParticipantId),
    KeyframeRequest {
        origin: ParticipantId,
        stream_id: StreamId,
        kind: KeyframeRequestKind,
    },
}

pub struct EventQueue<'a> {
    id: &'a ParticipantId,
    queue: &'a mut VecDeque<ParticipantEvent>,
}

impl<'a> EventQueue<'a> {
    pub fn new(id: &'a ParticipantId, queue: &'a mut VecDeque<ParticipantEvent>) -> Self {
        Self { id, queue }
    }

    pub fn subscribe(&mut self, stream_id: StreamId) {
        self.queue
            .push_back(ParticipantEvent::SubscribedTrack(*self.id, stream_id));
    }

    pub fn unsubscribe(&mut self, stream_id: StreamId) {
        self.queue
            .push_back(ParticipantEvent::UnsubscribedTrack(*self.id, stream_id));
    }

    pub fn publish_rtp(&mut self, stream_id: StreamId, pkt: RtpPacket) {
        self.queue
            .push_back(ParticipantEvent::PublishedRtp(stream_id, pkt));
    }

    pub fn publish_track(&mut self, track: Track) {
        self.queue
            .push_back(ParticipantEvent::PublishedTrack(track));
    }

    pub fn request_keyframe(&mut self, layer: &TrackLayer) {
        self.queue.push_back(ParticipantEvent::KeyframeRequest {
            origin: layer.meta.origin,
            stream_id: layer.stream_id(),
            kind: KeyframeRequestKind::Pli,
        });
    }

    pub fn update_deadline(&mut self, deadline: Instant) {
        self.queue
            .push_back(ParticipantEvent::NewDeadline(deadline, *self.id));
    }

    pub fn exit(&mut self) {
        self.queue.push_back(ParticipantEvent::Exited(*self.id));
    }
}
