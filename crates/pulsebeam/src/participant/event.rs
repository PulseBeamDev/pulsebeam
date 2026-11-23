use super::reverse::ReversePacket;
use crate::entity::PublicationKey;
use crate::keys::TrackHandle;
use crate::track::{Track, TrackMeta};

#[cfg(test)]
pub(crate) struct FabricPublication {
    pub source: crate::shard::router::FabricSourceHandle,
    pub arrival: pulsebeam_runtime::time::Instant,
    pub sequence: u64,
    pub packet: crate::route::fabric::local::OwnedPacket,
    pub reliable: Option<super::data_fabric::ReliableAdmission>,
}

pub(crate) struct FabricDataPosition {
    pub source: crate::shard::router::FabricSourceHandle,
    pub installation: pulsebeam_rtc::DataChannelInstallation,
    pub arrival: pulsebeam_runtime::time::Instant,
    pub sequence: u64,
    pub reliable: Option<super::data_fabric::ReliableAdmission>,
}

#[derive(Debug)]
pub(crate) struct FabricSubscriberFeedback {
    pub(crate) lease: super::data_fabric::SubscriberLease,
    pub(crate) kind: FabricFeedbackKind,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FabricFeedbackKind {
    Propagation(
        crate::route::fabric::local::DataExpectation,
        crate::route::fabric::local::PropagationOutcome,
    ),
    Assembled(crate::route::fabric::local::DataExpectation),
    Repair(crate::route::fabric::local::RepairRequest),
    CancelRepair(crate::route::fabric::local::RepairRequest),
}

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
    // The caller must fence local execution before requesting the compiled-plan cutoff.
    fn unpublish_owned_track(&mut self, meta: TrackMeta, reference: u64);
    fn set_owned_subscription(
        &mut self,
        reference: u64,
        source_room: crate::entity::ProjectRoomKey,
        track: crate::entity::PublicationKey,
        enabled: bool,
    );
    fn unsubscribe_owned(&mut self, reference: u64, source_room: crate::entity::ProjectRoomKey);
    fn request_reverse(&mut self, stream: TrackHandle, packet: ReversePacket);
    fn request_fabric_refresh(&mut self, branch: crate::shard::router::FabricLocalBranch);
    fn renew_authorization(&mut self, token: String);
    fn exit(&mut self);

    #[cfg(test)]
    fn publish_fabric_packet(&mut self, publication: FabricPublication);
    fn publish_fabric_data(
        &mut self,
        source: crate::shard::router::FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
        bytes: &[u8],
        now: pulsebeam_runtime::time::Instant,
        stamp: u32,
    ) -> Result<(), super::data_fabric::Error>;
    fn settle_fabric_original_unavailable(
        &mut self,
        source: crate::shard::router::FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
        demand: pulsebeam_proto::data_control::OriginalDemand,
        now: pulsebeam_runtime::time::Instant,
    );
    fn publish_fabric_media(
        &mut self,
        source: crate::shard::router::FabricSourceHandle,
        input: crate::rtp::fabric_ingress::MediaIngress,
    );
    fn fence_fabric_source(&mut self, source: crate::shard::router::FabricSourceHandle);
    fn stage_fabric_receiver(&mut self, work: crate::control::native::Preparation);
    fn stage_local_rtp_receiver(&mut self, reference: u64);
    fn observe_fabric_subscriber(&mut self, feedback: FabricSubscriberFeedback);
    fn stage_fabric_publisher(
        &mut self,
        work: crate::control::native::Preparation,
        publication: PublicationKey,
    );
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
        pub publish_track_calls: Vec<PublicationKey>,
        pub unpublish_track_calls: Vec<PublicationKey>,
        pub owned_subscriptions: Vec<(u64, PublicationKey, bool)>,
        pub owned_unsubscribes: Vec<u64>,
        pub reverse_requests: Vec<TrackHandle>,
        pub reverse_packets: Vec<ReversePacket>,
        pub exit_count: usize,
        pub fabric_publications: Vec<(crate::control::native::Preparation, PublicationKey)>,
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

        fn unpublish_owned_track(&mut self, meta: TrackMeta, _reference: u64) {
            self.unpublish_track(meta);
        }

        fn set_owned_subscription(
            &mut self,
            reference: u64,
            _source_room: crate::entity::ProjectRoomKey,
            track: PublicationKey,
            enabled: bool,
        ) {
            self.owned_subscriptions.push((reference, track, enabled));
        }
        fn unsubscribe_owned(
            &mut self,
            reference: u64,
            _source_room: crate::entity::ProjectRoomKey,
        ) {
            self.owned_unsubscribes.push(reference);
        }

        fn request_fabric_refresh(&mut self, _branch: crate::shard::router::FabricLocalBranch) {}

        fn request_reverse(&mut self, stream: TrackHandle, packet: ReversePacket) {
            self.reverse_requests.push(stream);
            self.reverse_packets.push(packet);
        }

        fn renew_authorization(&mut self, _token: String) {}

        fn exit(&mut self) {
            self.exit_count = self.exit_count.saturating_add(1);
        }

        fn publish_fabric_packet(&mut self, _publication: FabricPublication) {
            panic!("Fabric publications need a shard-owned sink");
        }

        fn publish_fabric_data(
            &mut self,
            _source: crate::shard::router::FabricSourceHandle,
            _installation: pulsebeam_rtc::DataChannelInstallation,
            _bytes: &[u8],
            _now: pulsebeam_runtime::time::Instant,
            _stamp: u32,
        ) -> Result<(), super::super::data_fabric::Error> {
            panic!("Fabric DATA requires the shard publication sink");
        }

        fn settle_fabric_original_unavailable(
            &mut self,
            _source: crate::shard::router::FabricSourceHandle,
            _installation: pulsebeam_rtc::DataChannelInstallation,
            _demand: pulsebeam_proto::data_control::OriginalDemand,
            _now: pulsebeam_runtime::time::Instant,
        ) {
            panic!("Fabric DATA requires the shard publication sink");
        }

        fn publish_fabric_media(
            &mut self,
            _source: crate::shard::router::FabricSourceHandle,
            _input: crate::rtp::fabric_ingress::MediaIngress,
        ) {
            panic!("Fabric media requires the shard publication sink");
        }

        fn fence_fabric_source(&mut self, _source: crate::shard::router::FabricSourceHandle) {
            panic!("Fabric publications need a shard-owned sink");
        }

        fn stage_fabric_receiver(&mut self, _work: crate::control::native::Preparation) {}

        fn stage_local_rtp_receiver(&mut self, _reference: u64) {}

        fn observe_fabric_subscriber(&mut self, _feedback: FabricSubscriberFeedback) {}

        fn stage_fabric_publisher(
            &mut self,
            work: crate::control::native::Preparation,
            publication: PublicationKey,
        ) {
            self.fabric_publications.push((work, publication));
        }
    }
}
