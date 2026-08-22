use super::packet::TrackPacket;
use crate::entity::{ParticipantId, TrackId};
use crate::keys::DownstreamSlotKey;
use crate::keys::TrackKey;
#[cfg(test)]
use crate::track::StreamId;
use crate::track::{DataLane, Topic, Track, TrackLayer, TrackMeta};
use str0m::channel::ChannelId;

pub trait ParticipantSink {
    fn connected(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        source_shard: crate::id::ShardId,
    );
    fn subscribe(&mut self, track: TrackMeta, slot: DownstreamSlotKey);
    fn unsubscribe(&mut self, track: TrackMeta, slot: DownstreamSlotKey);
    fn publish_track(&mut self, track: Track);
    fn unpublish_track(&mut self, track_id: TrackId);
    fn subscribe_data_topic(
        &mut self,
        topic: Topic,
        publisher: Option<crate::entity::ParticipantId>,
        channel: ChannelId,
        lane: DataLane,
    );
    fn unsubscribe_data_topic(
        &mut self,
        topic: Topic,
        publisher: Option<crate::entity::ParticipantId>,
        channel: ChannelId,
        lane: DataLane,
    );
    fn publish_data_topic(&mut self, topic: Topic, lane: DataLane);
    fn unpublish_data_topic(&mut self, topic: Topic, lane: DataLane);
    fn request_keyframe(&mut self, layer: &TrackLayer, fanout: Option<TrackKey>);
    fn exit(&mut self);

    fn publish_track_packet(&mut self, fanout: Option<TrackKey>, packet: TrackPacket);
    fn publish_sctp(&mut self, topic: Topic, stream: Option<TrackKey>, pkt: Vec<u8>);

    fn publish_reliable_sctp(&mut self, topic: Topic, stream: Option<TrackKey>, frame: Vec<u8>);
    fn forward_reliable_control(
        &mut self,
        publisher: ParticipantId,
        topic: Topic,
        stream: Option<TrackKey>,
        bytes: Vec<u8>,
    );
}

#[cfg(test)]
pub mod test_utils {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    #[derive(Debug, Default)]
    pub struct MockParticipantSink {
        pub subscribe_calls: Vec<(TrackMeta, DownstreamSlotKey)>,
        pub unsubscribe_calls: Vec<(TrackMeta, DownstreamSlotKey)>,
        pub publish_track_calls: Vec<TrackId>,
        pub unpublish_track_calls: Vec<TrackId>,
        pub subscribe_data_topic_calls: Vec<Topic>,
        pub unsubscribe_data_topic_calls: Vec<Topic>,
        pub publish_data_topic_calls: Vec<Topic>,
        pub unpublish_data_topic_calls: Vec<Topic>,
        pub request_keyframe_calls: Vec<(StreamId, crate::entity::ParticipantId)>,
        pub exit_count: usize,
        pub publish_track_packet_calls: Vec<TrackKey>,
        pub publish_sctp_calls: Vec<Topic>,
    }

    impl MockParticipantSink {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn reset(&mut self) {
            *self = Self::default();
        }
    }

    impl ParticipantSink for MockParticipantSink {
        fn connected(
            &mut self,
            _source: std::net::SocketAddr,
            _destination: std::net::SocketAddr,
            _source_shard: crate::id::ShardId,
        ) {
        }

        fn subscribe(&mut self, track: TrackMeta, slot: DownstreamSlotKey) {
            self.subscribe_calls.push((track, slot));
        }

        fn unsubscribe(&mut self, track: TrackMeta, slot: DownstreamSlotKey) {
            self.unsubscribe_calls.push((track, slot));
        }

        fn publish_track(&mut self, track: Track) {
            self.publish_track_calls.push(track.id());
        }

        fn unpublish_track(&mut self, track_id: TrackId) {
            self.unpublish_track_calls.push(track_id);
        }

        fn subscribe_data_topic(
            &mut self,
            topic: Topic,
            _publisher: Option<crate::entity::ParticipantId>,
            _channel: ChannelId,
            _lane: DataLane,
        ) {
            self.subscribe_data_topic_calls.push(topic);
        }

        fn unsubscribe_data_topic(
            &mut self,
            topic: Topic,
            _publisher: Option<crate::entity::ParticipantId>,
            _channel: ChannelId,
            _lane: DataLane,
        ) {
            self.unsubscribe_data_topic_calls.push(topic);
        }

        fn publish_data_topic(&mut self, topic: Topic, _lane: DataLane) {
            self.publish_data_topic_calls.push(topic);
        }

        fn unpublish_data_topic(&mut self, topic: Topic, _lane: DataLane) {
            self.unpublish_data_topic_calls.push(topic);
        }

        fn request_keyframe(&mut self, layer: &TrackLayer, _fanout: Option<TrackKey>) {
            self.request_keyframe_calls
                .push((layer.stream_id(), layer.meta.origin));
        }

        fn exit(&mut self) {
            self.exit_count = self.exit_count.saturating_add(1);
        }

        fn publish_track_packet(&mut self, fanout: Option<TrackKey>, _packet: TrackPacket) {
            if let Some(key) = fanout {
                self.publish_track_packet_calls.push(key);
            }
        }

        fn publish_sctp(&mut self, topic: Topic, _stream: Option<TrackKey>, _pkt: Vec<u8>) {
            self.publish_sctp_calls.push(topic);
        }

        fn publish_reliable_sctp(
            &mut self,
            _topic: Topic,
            _stream: Option<TrackKey>,
            _frame: Vec<u8>,
        ) {
        }
        fn forward_reliable_control(
            &mut self,
            _publisher: ParticipantId,
            _topic: Topic,
            _stream: Option<TrackKey>,
            _bytes: Vec<u8>,
        ) {
        }
    }
}
