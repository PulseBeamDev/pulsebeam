use std::collections::VecDeque;

use super::worker::ShardEvent;
use crate::entity::{ParticipantId, RoomId, TrackId};
use crate::keys::ParticipantKey;
use crate::keys::TrackKey;
use crate::participant::event::ParticipantSink;
use crate::participant::reverse::ReversePacket;
use crate::participant::{RoutedTrackPacket, TrackPacket};
use crate::track::{SelectionPolicy, Track, TrackMeta, TrackSelector};

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
    Binding(ParticipantBindingEvent),
    Lifecycle(ParticipantLifecycleEvent),
    Control(ShardEvent),
    Internal(ShardInternalEvent),
}

pub enum ParticipantBindingEvent {
    Activated {
        subscriber: ParticipantId,
        track: TrackMeta,
    },
    Deactivated {
        subscriber: ParticipantId,
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
    },
}

pub enum ShardInternalEvent {
    ReverseRequested {
        stream: TrackKey,
        packet: ReversePacket,
    },
}

pub(crate) struct EventPipeline {
    participant_events: VecDeque<ParticipantEvent>,
    track_queue: VecDeque<RoutedTrackPacket>,
    shard_events: VecDeque<ShardEvent>,
    packet_capacity: usize,
}

impl EventPipeline {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            participant_events: VecDeque::with_capacity(cap),
            track_queue: VecDeque::with_capacity(cap),
            shard_events: VecDeque::with_capacity(cap),
            packet_capacity: cap,
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

    fn push_packet(&mut self, packet: RoutedTrackPacket) -> bool {
        if self.track_queue.len() >= self.packet_capacity {
            metrics::counter!("routing_drop", "lane" => "packet", "stage" => "pipeline", "origin" => "local").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_drop("packet", "pipeline", "local");
            return false;
        }
        self.track_queue.push_back(packet);
        true
    }

    pub fn push_shard_event(&mut self, ev: ShardEvent) {
        self.shard_events.push_back(ev);
    }

    pub fn pop_packet(&mut self) -> Option<RoutedTrackPacket> {
        self.track_queue.pop_front()
    }

    pub fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.shard_events.pop_front()
    }

    pub fn has_pending(&self) -> bool {
        !self.participant_events.is_empty()
            || !self.track_queue.is_empty()
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
    fn activate_track(&mut self, track: TrackMeta) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Binding(
                ParticipantBindingEvent::Activated {
                    subscriber: self.id,
                    track,
                },
            ));
    }

    #[inline]
    fn deactivate_track(&mut self, track: TrackMeta) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Binding(
                ParticipantBindingEvent::Deactivated {
                    subscriber: self.id,
                    track,
                },
            ));
    }

    #[inline]
    fn publish_track(&mut self, track: Track) {
        let mut track = track;
        track.set_reverse(None);
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ShardEvent::TrackPublished {
                track,
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
    fn subscribe_tracks(&mut self, selector: TrackSelector, selection: SelectionPolicy) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::TrackSubscriptionAdded {
                    room_id: self.room_id,
                    subscriber: self.id,
                    selector,
                    selection,
                },
            ));
    }

    #[inline]
    fn unsubscribe_tracks(&mut self, selector: TrackSelector) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::TrackSubscriptionRemoved {
                    room_id: self.room_id,
                    subscriber: self.id,
                    selector,
                },
            ));
    }

    #[inline]
    fn request_reverse(&mut self, stream: TrackKey, packet: ReversePacket) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::ReverseRequested { stream, packet },
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
    fn publish_track_packet(&mut self, fanout: Option<TrackKey>, packet: TrackPacket) {
        let Some(key) = fanout else {
            return;
        };
        self.pipeline.push_packet(RoutedTrackPacket { key, packet });
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::entity::ExternalRoomId;
    use crate::track::DataLane;

    fn identity() -> SinkIdentity {
        let room = ExternalRoomId::new("room").unwrap();
        SinkIdentity {
            id: ParticipantId::new(),
            key: ParticipantKey::default(),
            room_id: RoomId::from_external(&room),
        }
    }

    /// RTP from every track kind uses the same queue and carries only its TrackKey.
    #[test]
    fn every_event_lands_in_its_own_queue() {
        let mut pipeline = EventPipeline::with_capacity(4);
        let who = identity();

        let mut sink = pipeline.participant_sink(who);
        sink.publish_track_packet(
            Some(TrackKey::default()),
            TrackPacket::Data {
                lane: DataLane::Realtime,
                bytes: vec![2],
            },
        );
        sink.publish_track_packet(
            Some(TrackKey::default()),
            TrackPacket::Data {
                lane: DataLane::Reliable,
                bytes: vec![3],
            },
        );
        sink.publish_track_packet(
            Some(TrackKey::default()),
            TrackPacket::Data {
                lane: DataLane::Realtime,
                bytes: vec![1],
            },
        );
        sink.exit();

        assert!(
            pipeline.pop_packet().is_some(),
            "RTP went to the track queue"
        );
        assert!(
            pipeline.pop_packet().is_some(),
            "RTP stayed in the track queue"
        );
        assert!(
            pipeline.pop_packet().is_some(),
            "data stayed in the packet pipeline"
        );
        assert!(
            pipeline.pop_participant_event().is_some(),
            "lifecycle went to participant events"
        );

        assert!(pipeline.pop_packet().is_none(), "and nothing crossed");
        assert!(pipeline.pop_packet().is_none());
    }

    /// Queues drain in the order they were filled. Media is a sequence, and reordering it here
    /// would be indistinguishable from reordering on the wire.
    #[test]
    fn a_queue_preserves_order() {
        let mut pipeline = EventPipeline::with_capacity(4);
        let who = identity();
        let mut sink = pipeline.participant_sink(who);
        for n in 0..3u8 {
            sink.publish_track_packet(
                Some(TrackKey::default()),
                TrackPacket::Data {
                    lane: DataLane::Realtime,
                    bytes: vec![n],
                },
            );
        }

        let drained: Vec<Vec<u8>> = std::iter::from_fn(|| pipeline.pop_packet())
            .map(|event| match event.packet {
                TrackPacket::Data { bytes, .. } => bytes,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(drained, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn the_generic_packet_pipeline_is_bounded_for_both_data_lanes() {
        let mut pipeline = EventPipeline::with_capacity(2);
        let who = identity();
        let key = TrackKey::default();
        let mut sink = pipeline.participant_sink(who);
        sink.publish_track_packet(
            Some(key),
            TrackPacket::Data {
                lane: DataLane::Realtime,
                bytes: vec![1],
            },
        );
        sink.publish_track_packet(
            Some(key),
            TrackPacket::Data {
                lane: DataLane::Reliable,
                bytes: vec![2],
            },
        );
        sink.publish_track_packet(
            Some(key),
            TrackPacket::Data {
                lane: DataLane::Realtime,
                bytes: vec![3],
            },
        );
        #[allow(
            clippy::drop_non_drop,
            reason = "end the mutable pipeline borrow before inspection"
        )]
        drop(sink);

        assert!(matches!(
            pipeline.pop_packet().unwrap().packet,
            TrackPacket::Data {
                lane: DataLane::Realtime,
                ..
            }
        ));
        assert!(matches!(
            pipeline.pop_packet().unwrap().packet,
            TrackPacket::Data {
                lane: DataLane::Reliable,
                ..
            }
        ));
        assert!(
            pipeline.pop_packet().is_none(),
            "the fixed packet capacity must be enforced"
        );
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
        });
        assert!(pipeline.has_pending(), "a shard event is work");
        assert!(pipeline.pop_shard_event().is_some());
        assert!(!pipeline.has_pending());

        pipeline.participant_sink(who).publish_track_packet(
            Some(TrackKey::default()),
            TrackPacket::Data {
                lane: DataLane::Realtime,
                bytes: vec![1],
            },
        );
        assert!(pipeline.has_pending(), "so is a track packet");
        assert!(pipeline.pop_packet().is_some());
        assert!(!pipeline.has_pending(), "and draining it clears the flag");
    }
}
