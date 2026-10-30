use super::packet::TrackPacket;
use super::reverse::ReversePacket;
#[cfg(test)]
use crate::entity::TrackId;
use crate::keys::TrackHandle;
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
    fn unpublish_track(&mut self, meta: TrackMeta);
    fn subscribe_tracks(&mut self, selector: TrackSelector, selection: SelectionPolicy);
    fn seed_subscription(&mut self, selector: TrackSelector, seed_id: u64);
    fn unsubscribe_tracks(&mut self, selector: TrackSelector);
    fn request_reverse(&mut self, stream: TrackHandle, packet: ReversePacket);
    fn renew_authorization(&mut self, token: String);
    fn exit(&mut self);

    fn publish_track_packet(&mut self, fanout: Option<TrackHandle>, packet: TrackPacket);
}

#[cfg(test)]
pub mod test_utils {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;

    #[derive(Debug, Default)]
    pub struct MockParticipantSink {
        pub connected_calls: Vec<(
            std::net::SocketAddr,
            std::net::SocketAddr,
            crate::id::ShardId,
        )>,
        pub activate_track_calls: Vec<TrackMeta>,
        pub deactivate_track_calls: Vec<TrackMeta>,
        pub publish_track_calls: Vec<TrackId>,
        pub unpublish_track_calls: Vec<TrackId>,
        pub reverse_requests: Vec<TrackHandle>,
        pub reverse_packets: Vec<ReversePacket>,
        pub exit_count: usize,
        pub publish_track_packet_calls: Vec<TrackHandle>,
    }

    impl MockParticipantSink {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl ParticipantSink for MockParticipantSink {
        fn connected(
            &mut self,
            source: std::net::SocketAddr,
            destination: std::net::SocketAddr,
            source_shard: crate::id::ShardId,
        ) {
            self.connected_calls
                .push((source, destination, source_shard));
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

        fn unpublish_track(&mut self, meta: TrackMeta) {
            self.unpublish_track_calls.push(meta.id);
        }

        fn subscribe_tracks(&mut self, _selector: TrackSelector, _selection: SelectionPolicy) {}
        fn seed_subscription(&mut self, _selector: TrackSelector, _seed_id: u64) {}

        fn unsubscribe_tracks(&mut self, _selector: TrackSelector) {}

        fn request_reverse(&mut self, stream: TrackHandle, packet: ReversePacket) {
            self.reverse_requests.push(stream);
            self.reverse_packets.push(packet);
        }

        fn renew_authorization(&mut self, _token: String) {}

        fn exit(&mut self) {
            self.exit_count = self.exit_count.saturating_add(1);
        }

        fn publish_track_packet(&mut self, fanout: Option<TrackHandle>, _packet: TrackPacket) {
            if let Some(key) = fanout {
                self.publish_track_packet_calls.push(key);
            }
        }
    }
}
