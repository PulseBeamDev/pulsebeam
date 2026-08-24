use super::packet::TrackPacket;
use super::reverse::ReversePacket;
use crate::entity::TrackId;
use crate::keys::TrackKey;
use crate::track::{SelectionPolicy, Track, TrackMeta, TrackSelector};

pub(crate) trait ParticipantSink {
    fn connected(
        &mut self,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        source_shard: crate::id::ShardId,
    );
    fn activate_track(&mut self, track: TrackMeta);
    fn deactivate_track(&mut self, track: TrackMeta);
    fn publish_track(&mut self, track: Track);
    fn unpublish_track(&mut self, track_id: TrackId);
    fn subscribe_tracks(&mut self, selector: TrackSelector, selection: SelectionPolicy);
    fn unsubscribe_tracks(&mut self, selector: TrackSelector);
    fn request_reverse(&mut self, stream: Option<TrackKey>, packet: ReversePacket);
    fn exit(&mut self);

    fn publish_track_packet(&mut self, fanout: Option<TrackKey>, packet: TrackPacket);
}

#[cfg(test)]
pub mod test_utils {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;

    #[derive(Debug, Default)]
    pub struct MockParticipantSink {
        pub activate_track_calls: Vec<TrackMeta>,
        pub deactivate_track_calls: Vec<TrackMeta>,
        pub publish_track_calls: Vec<TrackId>,
        pub unpublish_track_calls: Vec<TrackId>,
        pub reverse_requests: Vec<Option<TrackKey>>,
        pub exit_count: usize,
        pub publish_track_packet_calls: Vec<TrackKey>,
    }

    impl MockParticipantSink {
        pub fn new() -> Self {
            Self::default()
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

        fn activate_track(&mut self, track: TrackMeta) {
            self.activate_track_calls.push(track);
        }

        fn deactivate_track(&mut self, track: TrackMeta) {
            self.deactivate_track_calls.push(track);
        }

        fn publish_track(&mut self, track: Track) {
            self.publish_track_calls.push(track.id());
        }

        fn unpublish_track(&mut self, track_id: TrackId) {
            self.unpublish_track_calls.push(track_id);
        }

        fn subscribe_tracks(&mut self, _selector: TrackSelector, _selection: SelectionPolicy) {}

        fn unsubscribe_tracks(&mut self, _selector: TrackSelector) {}

        fn request_reverse(&mut self, stream: Option<TrackKey>, _packet: ReversePacket) {
            self.reverse_requests.push(stream);
        }

        fn exit(&mut self) {
            self.exit_count = self.exit_count.saturating_add(1);
        }

        fn publish_track_packet(&mut self, fanout: Option<TrackKey>, _packet: TrackPacket) {
            if let Some(key) = fanout {
                self.publish_track_packet_calls.push(key);
            }
        }
    }
}
