use crate::entity::{ParticipantId, TrackId, TrackKind};
use crate::keys::DownstreamSlotKey;
use crate::keys::{ReliableStreamKey, TrackKey, UnreliableStreamKey};
use crate::rtp::RtpPacket;
#[cfg(test)]
use crate::track::StreamId;
use crate::track::{Topic, Track, TrackLayer, TrackMeta};
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
    );
    fn unsubscribe_data_topic(
        &mut self,
        topic: Topic,
        publisher: Option<crate::entity::ParticipantId>,
        channel: ChannelId,
    );
    fn publish_data_topic(&mut self, topic: Topic);
    fn unpublish_data_topic(&mut self, topic: Topic);
    fn request_keyframe(&mut self, layer: &TrackLayer, fanout: Option<TrackKey>);
    fn exit(&mut self);

    fn publish_rtp(&mut self, fanout: Option<TrackKey>, kind: TrackKind, pkt: RtpPacket);
    fn publish_sctp(&mut self, topic: Topic, stream: Option<UnreliableStreamKey>, pkt: Vec<u8>);

    fn publish_reliable_data_topic(&mut self, topic: Topic);
    fn unpublish_reliable_data_topic(&mut self, topic: Topic);
    fn subscribe_reliable_data_topic(&mut self, topic: Topic, channel: ChannelId);
    fn unsubscribe_reliable_data_topic(&mut self, topic: Topic, channel: ChannelId);
    fn publish_reliable_sctp(
        &mut self,
        topic: Topic,
        stream: Option<ReliableStreamKey>,
        frame: Vec<u8>,
    );
    fn forward_reliable_control(
        &mut self,
        publisher: ParticipantId,
        topic: Topic,
        stream: Option<ReliableStreamKey>,
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
        pub publish_rtp_calls: Vec<TrackKey>,
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
        ) {
            self.subscribe_data_topic_calls.push(topic);
        }

        fn unsubscribe_data_topic(
            &mut self,
            topic: Topic,
            _publisher: Option<crate::entity::ParticipantId>,
            _channel: ChannelId,
        ) {
            self.unsubscribe_data_topic_calls.push(topic);
        }

        fn publish_data_topic(&mut self, topic: Topic) {
            self.publish_data_topic_calls.push(topic);
        }

        fn unpublish_data_topic(&mut self, topic: Topic) {
            self.unpublish_data_topic_calls.push(topic);
        }

        fn request_keyframe(&mut self, layer: &TrackLayer, _fanout: Option<TrackKey>) {
            self.request_keyframe_calls
                .push((layer.stream_id(), layer.meta.origin));
        }

        fn exit(&mut self) {
            self.exit_count = self.exit_count.saturating_add(1);
        }

        fn publish_rtp(&mut self, fanout: Option<TrackKey>, _kind: TrackKind, _pkt: RtpPacket) {
            if let Some(key) = fanout {
                self.publish_rtp_calls.push(key);
            }
        }

        fn publish_sctp(
            &mut self,
            topic: Topic,
            _stream: Option<UnreliableStreamKey>,
            _pkt: Vec<u8>,
        ) {
            self.publish_sctp_calls.push(topic);
        }

        fn publish_reliable_data_topic(&mut self, _topic: Topic) {}
        fn unpublish_reliable_data_topic(&mut self, _topic: Topic) {}
        fn subscribe_reliable_data_topic(&mut self, _topic: Topic, _channel: ChannelId) {}
        fn unsubscribe_reliable_data_topic(&mut self, _topic: Topic, _channel: ChannelId) {}
        fn publish_reliable_sctp(
            &mut self,
            _topic: Topic,
            _stream: Option<ReliableStreamKey>,
            _frame: Vec<u8>,
        ) {
        }
        fn forward_reliable_control(
            &mut self,
            _publisher: ParticipantId,
            _topic: Topic,
            _stream: Option<ReliableStreamKey>,
            _bytes: Vec<u8>,
        ) {
        }
    }
}
