use crate::entity::TrackId;
use crate::rtp::RtpPacket;
use crate::track::{StreamId, Topic, Track, TrackLayer, TrackMeta};
use tokio::time::Instant;

pub trait ParticipantSink {
    fn subscribe(&mut self, track: TrackMeta);
    fn unsubscribe(&mut self, track: TrackMeta);
    fn publish_track(&mut self, track: Track);
    fn unpublish_track(&mut self, track_id: TrackId);
    fn subscribe_data_topic(&mut self, topic: Topic, publisher: Option<crate::entity::ParticipantId>);
    fn unsubscribe_data_topic(&mut self, topic: Topic, publisher: Option<crate::entity::ParticipantId>);
    fn publish_data_topic(&mut self, topic: Topic);
    fn unpublish_data_topic(&mut self, topic: Topic);
    fn request_keyframe(&mut self, layer: &TrackLayer);
    fn update_deadline(&mut self, deadline: Instant);
    fn exit(&mut self);

    fn publish_rtp(&mut self, stream_id: StreamId, pkt: RtpPacket);
    fn publish_sctp(&mut self, topic: Topic, pkt: Vec<u8>);
}

#[cfg(test)]
pub mod test_utils {
    use super::*;

    #[derive(Debug, Default)]
    pub struct MockParticipantSink {
        pub subscribe_calls: Vec<TrackMeta>,
        pub unsubscribe_calls: Vec<TrackMeta>,
        pub publish_track_calls: Vec<TrackId>,
        pub unpublish_track_calls: Vec<TrackId>,
        pub subscribe_data_topic_calls: Vec<Topic>,
        pub unsubscribe_data_topic_calls: Vec<Topic>,
        pub publish_data_topic_calls: Vec<Topic>,
        pub unpublish_data_topic_calls: Vec<Topic>,
        pub request_keyframe_calls: Vec<(StreamId, crate::entity::ParticipantId)>,
        pub update_deadline_calls: Vec<Instant>,
        pub exit_count: usize,
        pub publish_rtp_calls: Vec<StreamId>,
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
        fn subscribe(&mut self, track: TrackMeta) {
            self.subscribe_calls.push(track);
        }

        fn unsubscribe(&mut self, track: TrackMeta) {
            self.unsubscribe_calls.push(track);
        }

        fn publish_track(&mut self, track: Track) {
            self.publish_track_calls.push(track.meta.id);
        }

        fn unpublish_track(&mut self, track_id: TrackId) {
            self.unpublish_track_calls.push(track_id);
        }

        fn subscribe_data_topic(&mut self, topic: Topic, _publisher: Option<crate::entity::ParticipantId>) {
            self.subscribe_data_topic_calls.push(topic);
        }

        fn unsubscribe_data_topic(&mut self, topic: Topic, _publisher: Option<crate::entity::ParticipantId>) {
            self.unsubscribe_data_topic_calls.push(topic);
        }

        fn publish_data_topic(&mut self, topic: Topic) {
            self.publish_data_topic_calls.push(topic);
        }

        fn unpublish_data_topic(&mut self, topic: Topic) {
            self.unpublish_data_topic_calls.push(topic);
        }

        fn request_keyframe(&mut self, layer: &TrackLayer) {
            self.request_keyframe_calls
                .push((layer.stream_id(), layer.meta.origin));
        }

        fn update_deadline(&mut self, deadline: Instant) {
            self.update_deadline_calls.push(deadline);
        }

        fn exit(&mut self) {
            self.exit_count += 1;
        }

        fn publish_rtp(&mut self, stream_id: StreamId, _pkt: RtpPacket) {
            self.publish_rtp_calls.push(stream_id);
        }

        fn publish_sctp(&mut self, topic: Topic, _pkt: Vec<u8>) {
            self.publish_sctp_calls.push(topic);
        }
    }
}
