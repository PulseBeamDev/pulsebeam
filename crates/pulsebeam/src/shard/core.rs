#[cfg(test)]
use crate::entity::EndpointKey;
use pulsebeam_queue as mailbox;
use pulsebeam_runtime::net;
use pulsebeam_runtime::time::Instant;
use slotmap::SecondaryMap;

use crate::clock::WallAnchor;
use crate::route::NodeTransportAddress;
#[cfg(test)]
use crate::route::fabric::local::OwnedPacket;
use crate::route::fabric::local::{Emission, Offers, PacketView, Rejection};
use crate::shard::events::{ParticipantBindingEvent, ParticipantEvent, ParticipantLifecycleEvent};
use crate::{
    id::ShardId,
    keys::ParticipantHandle,
    participant::{
        ConnectionConfig,
        batcher::{AppendStatus, GsoSendBatch, Handoff, NetworkEgress, OwnedPacketQueue},
    },
    shard::{
        dirty::DirtyTracker,
        events::EventPipeline,
        participants::ParticipantRegistry,
        router::{FabricLocalBranch, FabricSourceHandle, ShardRuntime},
        timer::TimerWheel,
        updates::ShardUpdateApplication,
    },
};

pub(crate) use super::router::IngressTransport;
use super::worker::{ShardCommand, ShardEvent, ShardFrame};

const PARTICIPANT_CAPACITY_HINT: usize = 64;

fn record_routing_drop(lane: &'static str, stage: &'static str, origin: &'static str) {
    metrics::counter!(
        "routing_drop",
        "lane" => lane,
        "stage" => stage,
        "origin" => origin
    )
    .increment(1);
    #[cfg(feature = "sim")]
    crate::sim_metrics::record_routing_drop(lane, stage, origin);
}

struct ShardNetworkEgress<'a> {
    udp: &'a mut GsoSendBatch,
}

impl NetworkEgress for ShardNetworkEgress<'_> {
    fn append_udp(&mut self, packets: &mut OwnedPacketQueue) -> AppendStatus {
        if packets.is_empty() {
            return AppendStatus::Drained;
        }
        if self.udp.is_full() {
            return AppendStatus::Full;
        }
        let _ = self.udp.append_from(packets);
        if packets.is_empty() {
            AppendStatus::Drained
        } else {
            AppendStatus::Full
        }
    }

    fn flush(&mut self) -> bool {
        // The shard submits after releasing the current participant borrow.
        false
    }
}

const FABRIC_CONTROL_TURN_BUDGET: usize = 16;
const PUBLISHER_LOOKUP_LIMIT: usize = 32;
const PUBLISHER_LOOKUP_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

struct PendingPublisherLookup {
    lookup: crate::route::fabric::relay::PublisherRepairLookup,
    result: pulsebeam_runtime::sync::oneshot::Sender<
        Option<crate::route::fabric::relay::LocatedRepair>,
    >,
    due: Instant,
}

enum PublisherLookupResult {
    Located(crate::route::fabric::relay::LocatedRepair),
    RetryAt(Instant),
}

pub(crate) struct ShardCore {
    execution: ShardExecution,
    updates: ShardUpdateApplication,
}

pub(crate) struct ShardExecution {
    pub(crate) shard_id: crate::id::ShardId,
    registry: ParticipantRegistry,
    pending_replacements:
        std::collections::VecDeque<(crate::route::NodeTransportAddress, ConnectionConfig)>,
    pub(super) runtime: ShardRuntime,
    pending_publisher_lookups: Vec<PendingPublisherLookup>,
    plans: SecondaryMap<crate::keys::TrackHandle, crate::shard::router::InstalledTrackPlan>,
    timers: TimerWheel,
    dirty: DirtyTracker,
    udp_send_batch: GsoSendBatch,
    pipeline: EventPipeline,
    quic_data_ready: bool,
    wall: WallAnchor,
    global_clock: Option<crate::clock::reference::ShardClock>,
}

impl ShardCore {
    pub(crate) fn new(
        shard_id: impl Into<crate::id::ShardId>,
        max_gso_segments: usize,
        shard_count: usize,
        wall: WallAnchor,
        update_rx: mailbox::Receiver<Box<crate::shard_update::ShardUpdate>>,
    ) -> Self {
        let shard_id = shard_id.into();
        Self {
            execution: ShardExecution::new(shard_id, max_gso_segments, shard_count, wall),
            updates: ShardUpdateApplication::new(shard_id, update_rx),
        }
    }

    pub(crate) fn set_global_clock(&mut self, snapshot: crate::clock::reference::Snapshot) {
        match &mut self.execution.global_clock {
            Some(clock) => clock.update(snapshot),
            clock @ None => *clock = Some(crate::clock::reference::ShardClock::new(snapshot)),
        }
    }

    pub(super) fn register_quic_owner(
        &mut self,
        connection: crate::entity::ConnectionId,
        transport: crate::route::NodeTransportAddress,
        association: std::num::NonZeroU64,
        grant: super::participants::QuicGrant,
    ) -> bool {
        self.execution
            .registry
            .insert_quic(connection, transport, association, grant)
    }

    pub(super) fn update_quic_grant(
        &mut self,
        connection: crate::entity::ConnectionId,
        revision: u64,
        expiry: Option<i64>,
    ) {
        if let Some(expiry) = expiry.and_then(|expiry| u64::try_from(expiry).ok()) {
            self.execution.registry.update_quic_grant(
                connection,
                super::participants::QuicGrant {
                    revision,
                    expiry: pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(expiry),
                },
            );
        }
    }

    pub(super) fn retire_quic_owner(
        &mut self,
        connection: crate::entity::ConnectionId,
        transport: crate::route::NodeTransportAddress,
        association: std::num::NonZeroU64,
    ) {
        self.execution
            .registry
            .retire_quic(connection, transport, association);
    }

    pub(super) fn quic_owner_handle(
        &self,
        transport: crate::route::NodeTransportAddress,
    ) -> Option<ParticipantHandle> {
        self.execution.registry.resolve_transport(transport)
    }

    pub(super) fn install_quic_data_source(
        &mut self,
        publisher: ParticipantHandle,
        publication: crate::entity::PublicationKey,
        installation: crate::participant::data_fabric::QuicInstallation,
        clock: Option<pulsebeam_proto::data_record::Clock<'_>>,
        emission: Emission,
    ) -> Option<(FabricSourceHandle, std::num::NonZeroU64)> {
        let runtime = &mut self.execution.runtime;
        if runtime.track_handle(publication).is_some() {
            return None;
        }
        let track = runtime.install_track(publication, None, Some(publisher), None);
        let result = (|| {
            let source = runtime.install_fabric_source(track, publisher, emission, None, None)?;
            let incarnation = runtime.fabric_incarnation(source)?;
            runtime
                .bind_quic_data_publisher(source, publisher, installation, clock)
                .then_some((source, incarnation))
        })();
        if result.is_none() {
            runtime.retire_track(publication);
        }
        result
    }

    pub(super) fn install_quic_rtp_source(
        &mut self,
        publisher: ParticipantHandle,
        publication: crate::entity::PublicationKey,
        association: std::num::NonZeroU64,
        enabled: bool,
        encoding: crate::route::fabric::RtpSourceEncoding,
        emission: Emission,
    ) -> Option<(FabricSourceHandle, std::num::NonZeroU64)> {
        use pulsebeam_core::fabric::media_source::{Anchors, Limits, MediaIdentity};
        let identity = MediaIdentity::new(
            emission.profile(),
            Anchors::default(),
            Limits {
                positions: std::num::NonZeroU16::new(64)?,
                max_age: std::time::Duration::from_millis(120),
                max_idle: std::time::Duration::from_secs(2),
            },
        )?;
        let now = Instant::now();
        let reference = self
            .execution
            .global_clock
            .as_mut()
            .and_then(|clock| clock.now(now))
            .map(|estimate| (now, estimate));
        let primary = *encoding.primary;
        let runtime = &mut self.execution.runtime;
        if runtime.track_handle(publication).is_some() {
            return None;
        }
        let track = runtime.install_track(publication, None, Some(publisher), None);
        let result = (|| {
            let refresh = (emission.profile() == pulsebeam_core::fabric::Profile::H264)
                .then(|| std::num::NonZeroU32::new(*encoding.primary))
                .flatten();
            let source = runtime.install_fabric_source(
                track,
                publisher,
                emission,
                refresh,
                Some(encoding),
            )?;
            let incarnation = runtime.fabric_incarnation(source)?;
            (runtime
                .bind_fabric_media_identity(source, publisher, identity, primary, reference, None)
                && runtime.set_quic_media_binding(source, association, enabled))
            .then_some((source, incarnation))
        })();
        if result.is_none() {
            runtime.retire_track(publication);
        }
        result
    }

    pub(super) fn set_quic_media_enabled(
        &mut self,
        source: FabricSourceHandle,
        association: std::num::NonZeroU64,
        enabled: bool,
    ) -> bool {
        self.execution
            .runtime
            .set_quic_media_binding(source, association, enabled)
    }

    pub(super) fn ingest_quic_sender_report(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        report: pulsebeam_agent_quic::records::RtcpSenderReport,
        now: Instant,
    ) {
        if self
            .execution
            .fabric_publisher_authorized(publisher, source)
        {
            let [a, b, c, d, e, f, g, h] = report.ntp.to_be_bytes();
            self.execution.runtime.observe_fabric_sender_report(
                source,
                publisher,
                now,
                pulsebeam_rtc::SenderReport {
                    arrival: now.into_std(),
                    ssrc: report.ssrc.get(),
                    ntp_seconds: u32::from_be_bytes([a, b, c, d]),
                    ntp_fraction: u32::from_be_bytes([e, f, g, h]),
                    rtp_timestamp: report.rtp_timestamp,
                },
            );
        }
    }

    pub(super) fn ingest_quic_media_packet(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        ingress: crate::rtp::fabric_ingress::QuicRtpIngress,
        packet: &pulsebeam_core::fabric::BorrowedPacket<'_>,
        now: Instant,
    ) {
        self.execution
            .ingest_quic_media_packet(source, publisher, ingress, packet, now);
    }

    pub(super) fn retire_quic_publication(&mut self, publication: crate::entity::PublicationKey) {
        let execution = &mut self.execution;
        if let Some(track) = execution.runtime.track_handle(publication) {
            execution.plans.remove(track);
            execution.runtime.retire_track(publication);
        }
    }

    pub(super) fn ingest_quic_data(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        installation: crate::participant::data_fabric::QuicInstallation,
        input: crate::participant::data_fabric::QuicDataInput<'_>,
        now: Instant,
    ) -> Option<u64> {
        let execution = &mut self.execution;
        if !execution.fabric_publisher_authorized(publisher, source) {
            return None;
        }
        let data_time = crate::participant::data_fabric::DataTime::from_reference(
            execution
                .global_clock
                .as_mut()
                .and_then(|clock| clock.now(now)),
        );
        let position = execution
            .runtime
            .admit_quic_data(source, publisher, installation, input, now, data_time)
            .ok()?;
        if let Some(admission) = position.reliable {
            let unix_now = execution.fabric_unix_time();
            let registry = &execution.registry;
            if !execution.runtime.accept_quic_data_expectation(
                source,
                publisher,
                admission,
                now,
                |branch, destination| {
                    registry
                        .reliable_receiver(destination, branch, unix_now)
                        .map(|lease| lease.endpoint.branch)
                },
            ) {
                return None;
            }
        }
        if let Ok(report) = execution
            .runtime
            .offer_quic_fabric_data(publisher, now, &position)
        {
            execution.drain_after_data_offer(
                now,
                source,
                position.sequence,
                position.reliable,
                &report,
            );
        }
        position
            .reliable
            .filter(|admission| admission.acknowledge)
            .map(|admission| admission.unit.unit_seq)
    }

    pub(super) fn take_quic_data_ready(&mut self) -> bool {
        std::mem::take(&mut self.execution.quic_data_ready)
    }

    pub(super) fn pop_quic_publisher_refresh(
        &mut self,
        publisher: ParticipantHandle,
    ) -> Option<(FabricSourceHandle, u32, Instant)> {
        self.execution
            .registry
            .pop_quic_publisher_refresh(publisher)
    }

    pub(super) fn pop_quic_publisher_control(
        &mut self,
        publisher: ParticipantHandle,
    ) -> Option<(
        FabricSourceHandle,
        pulsebeam_proto::data_control::publisher_observation::Record,
    )> {
        let execution = &mut self.execution;
        while let Some((source, record)) = execution.registry.pop_quic_publisher_control(publisher)
        {
            if execution.fabric_publisher_authorized(publisher, source) {
                return Some((source, record));
            }
        }
        None
    }

    pub(super) fn settle_quic_original_unavailable(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        demand: pulsebeam_proto::data_control::OriginalDemand,
        now: Instant,
    ) {
        let execution = &mut self.execution;
        if execution.fabric_publisher_authorized(publisher, source) {
            execution
                .runtime
                .settle_quic_original_unavailable(source, publisher, demand, now);
        }
    }

    pub(super) fn take_quic_subscriber_control(
        &mut self,
        receiver: ParticipantHandle,
    ) -> Option<Vec<u8>> {
        use crate::shard::router::FabricOwner;
        use pulsebeam_proto::state::codec;
        let execution = &mut self.execution;
        let unix_now = execution.fabric_unix_time();
        while let Some((lease, record)) = execution.registry.pop_quic_subscriber_control(receiver) {
            if execution
                .registry
                .reliable_receiver(receiver, lease.branch, unix_now)
                != Some(lease)
            {
                continue;
            }
            let authorized = match execution
                .runtime
                .fabric_handoff_owner(lease.branch, receiver)
            {
                Some(FabricOwner::Local(publisher)) => {
                    execution.fabric_publisher_authorized(publisher, lease.branch.source())
                }
                Some(FabricOwner::Shard(_) | FabricOwner::Network) => true,
                None => false,
            };
            if authorized
                && let Ok(frame) = lease.observation(record)
                && let Ok(bytes) = codec::encode_data_control(&frame)
            {
                return Some(bytes);
            }
        }
        None
    }

    pub(super) fn install_quic_rtp_branch(
        &mut self,
        receiver: ParticipantHandle,
        publication: crate::entity::PublicationKey,
        emission: Emission,
        target: Option<pulsebeam_core::dd::evidence::DecodeTarget>,
    ) -> Option<FabricLocalBranch> {
        self.execution
            .install_quic_rtp_branch(receiver, publication, emission, target)
    }

    pub(super) fn request_quic_receiver_refresh(
        &mut self,
        receiver: ParticipantHandle,
        branch: FabricLocalBranch,
    ) {
        self.execution
            .pipeline
            .request_fabric_refresh(receiver, branch);
    }

    pub(super) fn quic_rtp_branch_live(
        &mut self,
        branch: FabricLocalBranch,
        receiver: ParticipantHandle,
    ) -> bool {
        self.execution.quic_rtp_branch_live(branch, receiver)
    }

    pub(super) fn admit_quic_rtp(
        &mut self,
        branch: FabricLocalBranch,
        receiver: ParticipantHandle,
        now: Instant,
        write: impl FnOnce(
            u64,
            crate::route::fabric::local::PacketView<'_>,
            crate::route::fabric::local::OutputEvidence,
        ) -> Result<(), ()>,
    ) -> Result<Option<()>, ()> {
        self.execution.admit_quic_rtp(branch, receiver, now, write)
    }

    pub(super) fn reject_filtered_quic_rtp(
        &mut self,
        branch: FabricLocalBranch,
        receiver: ParticipantHandle,
        now: Instant,
    ) {
        self.execution.reset_fabric_output(receiver, branch);
        self.execution.request_fabric_refresh(receiver, branch, now);
    }

    pub(super) fn install_quic_data_branch(
        &mut self,
        receiver: ParticipantHandle,
        publication: crate::entity::PublicationKey,
        endpoint: pulsebeam_proto::data_control::SubscriberBinding,
        reliable_receiver: Option<(u64, u64)>,
        emission: Emission,
    ) -> Option<FabricLocalBranch> {
        let execution = &mut self.execution;
        let track = execution.runtime.track_handle(publication)?;
        let source = execution.runtime.fabric_source_for_track(track)?;
        let authorized = match execution.runtime.source_origin(track) {
            Some(publisher) => execution.fabric_publisher_authorized(publisher, source),
            None => execution.runtime.is_shard_fabric_source(source),
        };
        if execution.runtime.fabric_incarnation(source) != Some(endpoint.source) || !authorized {
            return None;
        }
        let branch = execution.runtime.install_fabric_branch(
            track,
            source,
            receiver,
            emission,
            execution.plans.get(track)?,
            None,
        )?;
        if let Some((reference, owned)) = reliable_receiver {
            if !execution.registry.bind_quic_data_receiver(
                receiver,
                crate::participant::data_fabric::SubscriberLease {
                    owned,
                    branch,
                    endpoint,
                },
            ) {
                execution.runtime.retire_fabric_branch(branch, receiver);
                return None;
            }
            execution.announce_shard_data_receiver(receiver, branch, reference);
        }
        Some(branch)
    }

    pub(super) fn retire_quic_data_branch(
        &mut self,
        branch: FabricLocalBranch,
        receiver: ParticipantHandle,
    ) {
        self.execution
            .registry
            .unbind_quic_data_receiver(receiver, branch);
        self.execution
            .runtime
            .retire_fabric_branch(branch, receiver);
    }

    pub(super) fn admit_quic_receiver_data(
        &mut self,
        branch: FabricLocalBranch,
        receiver: ParticipantHandle,
        now: Instant,
        reliable: bool,
        write: impl FnOnce(
            u64,
            crate::route::fabric::local::PacketView<'_>,
            Option<crate::route::fabric::local::RepairRequest>,
        ) -> crate::route::fabric::local::DataWrite,
    ) -> bool {
        use crate::{route::fabric::local::DataWrite, shard::router::FabricOwner};
        let execution = &mut self.execution;
        let publication = match execution.runtime.fabric_handoff_owner(branch, receiver) {
            Some(FabricOwner::Local(publisher))
                if execution.fabric_publisher_authorized(publisher, branch.source()) =>
            {
                execution
                    .runtime
                    .local_fabric_publication(branch.source(), publisher)
            }
            Some(FabricOwner::Shard(binding)) => Some(binding.publication),
            _ => None,
        };
        let member = publication
            .and_then(|publication| execution.runtime.track_handle(publication))
            .and_then(|track| execution.plans.get(track))
            .is_some_and(|plan| plan.local.contains(&receiver));
        if !member {
            return false;
        }
        let lease = if reliable {
            let Some(lease) = execution.registry.reliable_receiver(
                receiver,
                branch,
                execution.fabric_unix_time(),
            ) else {
                return false;
            };
            Some(lease)
        } else {
            None
        };
        let current = lease.map(|lease| lease.endpoint.branch);
        let mut had_repair = false;
        let Some(handoff) = execution.runtime.admit_fabric_data(
            branch,
            receiver,
            now,
            current,
            |sequence, packet, repair| {
                had_repair = repair.is_some();
                if had_repair && !reliable {
                    DataWrite::Rejected
                } else {
                    write(sequence, packet, repair)
                }
            },
        ) else {
            return false;
        };
        if let Some((request, lease)) = handoff.completed.zip(lease) {
            execution.report_fabric_repair(
                receiver,
                lease,
                request,
                pulsebeam_proto::data_control::RepairStatus::HandedOff,
            );
        }
        handoff.written || !had_repair
    }

    pub(super) fn current_quic_grant(
        &self,
        connection: crate::entity::ConnectionId,
        transport: crate::route::NodeTransportAddress,
        association: std::num::NonZeroU64,
    ) -> Option<(super::participants::QuicGrant, u64)> {
        let unix_now = self
            .execution
            .wall
            .execution_time()
            .wall
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let grant =
            self.execution
                .registry
                .quic_grant(connection, transport, association, unix_now)?;
        Some((grant, unix_now))
    }

    pub(super) fn receive_quic_data_control(
        &mut self,
        connection: crate::entity::ConnectionId,
        transport: crate::route::NodeTransportAddress,
        association: std::num::NonZeroU64,
        frame: pulsebeam_proto::state::NativeDataControl,
        now: Instant,
    ) {
        if self
            .current_quic_grant(connection, transport, association)
            .is_none()
        {
            return;
        }
        let execution = &mut self.execution;
        let Some(key) = execution.registry.resolve_transport(transport) else {
            return;
        };
        let Some(pulsebeam_proto::state::native_data_control::Endpoint::Subscriber(frame)) =
            frame.endpoint
        else {
            return;
        };
        let Some(lease) = execution.registry.quic_receiver_lease(key, frame.owned) else {
            return;
        };
        if let Some(feedback) =
            crate::participant::event::FabricSubscriberFeedback::decode(lease, &frame.record)
        {
            execution.observe_fabric_subscriber(key, feedback, now, Some(connection));
        }
    }

    pub(crate) fn observe_execution_time(&mut self, at: net::ReceiveTime) -> net::ReceiveTime {
        self.execution.wall.observe_execution(at)
    }

    pub(super) fn apply_updates_with_retirement(
        &mut self,
        budget: usize,
        retire: impl FnMut(crate::entity::ConnectionId, crate::route::NodeTransportAddress),
    ) -> usize {
        self.updates
            .apply_with_retirement(&mut self.execution, budget, retire)
    }

    #[cfg(test)]
    pub(crate) fn apply_updates(&mut self, budget: usize) -> usize {
        self.updates.apply(&mut self.execution, budget)
    }

    pub(crate) async fn update_readable(&mut self) -> bool {
        self.updates.readable().await
    }

    pub(crate) fn on_command(
        &mut self,
        cmd: ShardCommand,
        router: &impl IngressTransport,
    ) -> Option<()> {
        let result = self.execution.on_command(cmd, router);
        self.updates
            .apply_pending_participant_effects(&mut self.execution);
        result
    }

    pub(crate) fn on_shard_frames(
        &mut self,
        frames: impl IntoIterator<Item = ShardFrame>,
        now: Instant,
        router: &impl IngressTransport,
    ) {
        self.execution.on_shard_frames(frames, now, router);
    }

    pub(super) fn on_network_fabric(
        &mut self,
        address: crate::route::NodeRouteAddress,
        ingress: pulsebeam_link::FabricIngress,
        now: Instant,
        arrival: Instant,
        image: crate::route::fabric::NetworkPacket,
    ) {
        let repair = ingress.purpose == pulsebeam_link::FabricPurpose::RepairOnly;
        let (ingress, image) = if repair {
            if image.window.profile() != pulsebeam_core::fabric::Profile::Data {
                let Some(repaired) = self
                    .execution
                    .runtime
                    .take_media_repair(address, ingress, image, now)
                else {
                    return;
                };
                repaired
            } else {
                self.execution
                    .receive_network_repair(address, ingress, now, image);
                return;
            }
        } else {
            (ingress, image)
        };
        let profile = image.packet.header().profile;
        let data = profile == pulsebeam_core::fabric::Profile::Data;
        #[cfg(feature = "sim")]
        let wan_protected_bytes = image.packet.wan_received_bytes();
        let position = image.position;
        let capture = image.capture;
        let result = self
            .execution
            .runtime
            .offer_live_network(address, ingress, now, arrival, image);
        if let Ok(report) = result
            && let Some(source) = self.execution.runtime.network_fabric_source(address)
        {
            if !data {
                if let Some(capture) = capture
                    && report.enqueued != 0
                {
                    Self::record_wan_capture_age(
                        self.execution.global_clock.as_ref(),
                        capture,
                        arrival,
                        "ingress",
                    );
                }
                #[cfg(feature = "sim")]
                if profile == pulsebeam_core::fabric::Profile::Opus && report.enqueued != 0 {
                    crate::sim_metrics::record_routing_counter("fabric_network_opus_receive");
                    if let Some(bytes) = wan_protected_bytes {
                        crate::sim_metrics::record_wan_opus_wire_bytes(usize::from(bytes.get()));
                    }
                }
                #[cfg(feature = "sim")]
                if profile == pulsebeam_core::fabric::Profile::Opus {
                    crate::sim_metrics::wan_repair::admitted(
                        ingress.association,
                        position,
                        repair,
                        report.enqueued,
                    );
                }
                if let Some(interest) = self
                    .execution
                    .runtime
                    .observe_network_media_position(address, ingress, position, now)
                {
                    self.execution.pipeline.push_network_media_repair(interest);
                }
                self.execution.drain_fabric_rtp(source, now);
            } else if report.enqueued != 0 {
                self.execution.drain_fabric_data(source, now);
            }
        }
    }

    pub(crate) fn on_udp_batch_routed(
        &mut self,
        batch: net::RecvPacketBatch,
        router: &impl IngressTransport,
    ) {
        self.execution.on_udp_batch_routed(batch, router);
    }

    fn record_wan_capture_age(
        clock: Option<&crate::clock::reference::ShardClock>,
        capture: pulsebeam_clock::deadline::Timing,
        observed_at: Instant,
        stage: &'static str,
    ) {
        use pulsebeam_clock::{deadline, domain::Quality};
        let age = clock.and_then(|clock| {
            let reference = clock.estimate_at(observed_at)?;
            let uncertainty_ns = reference.uncertainty_ns?;
            let now = deadline::Timing {
                instant: reference.global,
                absolute_uncertainty_ns: Some(uncertainty_ns),
                quality: Quality::Synchronized,
            };
            deadline::capture_age(capture, now).ok().flatten()
        });
        let limit = crate::route::fabric::network_repair::MAX_REPAIR_LIFETIME;
        if let Some(age) = age
            && let Ok(latest) = u64::try_from(age.latest.as_nanos())
            && std::time::Duration::from_nanos(latest) < limit
        {
            metrics::histogram!("wan_capture_age_upper_seconds", "stage" => stage)
                .record(std::time::Duration::from_nanos(latest).as_secs_f64());
        } else {
            metrics::counter!("wan_capture_age_inconclusive", "stage" => stage).increment(1);
        }
    }

    pub(crate) fn on_tcp_ingress(&mut self, event: crate::ice_tcp::TcpIngress) {
        use crate::ice_tcp::TcpIngress;
        let route = match &event {
            TcpIngress::Adopt { flow, .. } => flow.route,
            TcpIngress::Frame(packet) => packet.route,
            TcpIngress::Closed { route } => *route,
        };
        if route.address.shard() != self.execution.shard_id {
            return;
        }
        let Some(key) = self.execution.registry.resolve_transport(route.address) else {
            return;
        };
        let Some(participant) = self.execution.registry.resolve_mut(key) else {
            return;
        };
        match event {
            TcpIngress::Adopt {
                flow,
                first_stun,
                reply,
            } => {
                let accepted =
                    participant.bind_tcp_flow(flow) && participant.enqueue_tcp(first_stun);
                let _ = reply.send(accepted);
            }
            TcpIngress::Frame(packet) => {
                participant.enqueue_tcp(packet);
            }
            TcpIngress::Closed { .. } => participant.retire_tcp_flow(route),
        }
        self.execution.dirty.mark(key, participant);
    }

    pub(crate) fn flush_stream_buffers(&mut self, now: Instant, budget: usize) -> usize {
        self.execution.flush_stream_buffers(now, budget)
    }

    pub(crate) fn flush_participant_events(
        &mut self,
        router: &impl IngressTransport,
        budget: usize,
    ) -> usize {
        self.execution.flush_participant_events(router, budget)
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        self.execution.has_pending_events()
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.execution.pop_shard_event()
    }

    pub(crate) fn participant_count(&self) -> usize {
        self.execution.participant_count()
    }

    #[cfg(test)]
    pub(crate) fn installed_route_count(&self) -> usize {
        self.execution.runtime.routes.len()
    }

    pub(crate) fn next_timer_deadline(&mut self) -> Option<Instant> {
        if !self.execution.dirty.is_empty() {
            Some(Instant::now())
        } else {
            self.execution.next_timer_deadline()
        }
    }

    pub(crate) fn poll_and_flush_dirty(
        &mut self,
        udp_socket: &mut pulsebeam_udp::UdpSocket,
        budget: usize,
        router: &impl IngressTransport,
    ) -> usize {
        self.execution
            .poll_and_flush_dirty(udp_socket, budget, router)
    }

    pub(crate) fn drain_pending_close(
        &mut self,
    ) -> impl Iterator<Item = (NodeTransportAddress, Vec<std::net::SocketAddr>)> + '_ {
        self.execution.registry.drain_pending_close()
    }
}

impl ShardExecution {
    fn install_quic_rtp_branch(
        &mut self,
        receiver: ParticipantHandle,
        publication: crate::entity::PublicationKey,
        emission: Emission,
        target: Option<pulsebeam_core::dd::evidence::DecodeTarget>,
    ) -> Option<FabricLocalBranch> {
        let track = self.runtime.track_handle(publication)?;
        let source = self.runtime.fabric_source_for_track(track)?;
        if let Some(publisher) = self.runtime.source_origin(track) {
            if !self.fabric_publisher_authorized(publisher, source) {
                return None;
            }
        } else if !self.runtime.is_shard_fabric_source(source) {
            return None;
        }
        self.runtime.install_fabric_branch(
            track,
            source,
            receiver,
            emission,
            self.plans.get(track)?,
            target,
        )
    }

    fn quic_rtp_branch_live(
        &mut self,
        branch: FabricLocalBranch,
        receiver: ParticipantHandle,
    ) -> bool {
        use crate::shard::router::FabricOwner;
        let publication = match self.runtime.fabric_handoff_owner(branch, receiver) {
            Some(FabricOwner::Local(publisher))
                if self.fabric_publisher_authorized(publisher, branch.source()) =>
            {
                self.runtime
                    .local_fabric_publication(branch.source(), publisher)
            }
            Some(FabricOwner::Shard(binding)) => Some(binding.publication),
            _ => None,
        };
        publication
            .and_then(|id| self.runtime.track_handle(id))
            .and_then(|track| self.plans.get(track))
            .is_some_and(|plan| plan.local.contains(&receiver))
    }

    fn admit_quic_rtp(
        &mut self,
        branch: FabricLocalBranch,
        receiver: ParticipantHandle,
        now: Instant,
        write: impl FnOnce(
            u64,
            crate::route::fabric::local::PacketView<'_>,
            crate::route::fabric::local::OutputEvidence,
        ) -> Result<(), ()>,
    ) -> Result<Option<()>, ()> {
        if !self.quic_rtp_branch_live(branch, receiver) {
            return Ok(None);
        }
        self.runtime.admit_fabric(branch, receiver, now, write)
    }

    fn ingest_quic_media_packet(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        ingress: crate::rtp::fabric_ingress::QuicRtpIngress,
        packet: &pulsebeam_core::fabric::BorrowedPacket<'_>,
        now: Instant,
    ) {
        if !self.fabric_publisher_authorized(publisher, source) {
            return;
        }
        let Ok(input) = ingress.prepare_packet(now.into_std(), packet) else {
            return;
        };
        let reference = self.global_clock.as_mut().and_then(|clock| clock.now(now));
        if let Ok(report) =
            self.runtime
                .offer_fabric_media(source, publisher, now, &input, reference.as_ref())
        {
            self.drain_fabric_rtp(source, now);
            self.refresh_after_fabric_offer(source, now, &report);
        }
    }

    fn new(
        shard_id: crate::id::ShardId,
        max_gso_segments: usize,
        shard_count: usize,
        wall: WallAnchor,
    ) -> Self {
        debug_assert!(shard_count > 0);
        // A node cannot bind more sockets than `PackedRoute` can address, and
        // the route's shard field is 12 bits — so this cannot overflow. Clamp
        // rather than panic: a shard that mis-sizes its own steering table
        // should drop packets it cannot own, not take the process down.
        let shard_count = u16::try_from(shard_count).unwrap_or(u16::MAX);
        debug_assert!(shard_count > 0, "a node always has at least one shard");
        let runtime = ShardRuntime::new(shard_id);
        Self {
            shard_id,
            registry: ParticipantRegistry::new(shard_id, max_gso_segments, shard_count),
            pending_replacements: std::collections::VecDeque::new(),
            pending_publisher_lookups: Vec::new(),
            runtime,
            plans: SecondaryMap::new(),
            timers: TimerWheel::new(PARTICIPANT_CAPACITY_HINT),
            dirty: DirtyTracker::with_capacity(PARTICIPANT_CAPACITY_HINT),
            udp_send_batch: GsoSendBatch::preallocated(),
            pipeline: EventPipeline::with_capacity(PARTICIPANT_CAPACITY_HINT),
            quic_data_ready: false,
            wall,
            global_clock: None,
        }
    }

    fn drain_participant_network(
        &mut self,
        key: ParticipantHandle,
        udp_socket: &mut pulsebeam_udp::UdpSocket,
    ) {
        self.dispatch_staged_media(key);
        loop {
            let Some(participant) = self.registry.resolve_mut(key) else {
                return;
            };
            let mut egress = ShardNetworkEgress {
                udp: &mut self.udp_send_batch,
            };
            participant.drain_network(&mut egress);
            if !participant.network_pending() || !self.flush_udp(udp_socket) {
                return;
            }
        }
    }

    fn dispatch_staged_media(&mut self, key: ParticipantHandle) {
        if let Some(handoff) = self
            .registry
            .resolve_mut(key)
            .and_then(|participant| participant.staged_handoff(key))
        {
            let unix_now = self.fabric_unix_time();
            let authorized =
                Self::authorize_media_handoff(&self.runtime, &mut self.registry, handoff, unix_now);
            if let Some(participant) = self.registry.resolve_mut(key) {
                participant.dispatch_handoff(handoff, authorized);
            }
        }
    }

    fn flush_udp(&mut self, socket: &mut pulsebeam_udp::UdpSocket) -> bool {
        let unix_now = self.fabric_unix_time();
        let runtime = &self.runtime;
        let registry = &mut self.registry;
        let submission = self.udp_send_batch.flush_checked(socket, |handoff| {
            Self::authorize_media_handoff(runtime, registry, handoff, unix_now)
        });
        self.resolve_media_submissions(submission.handoffs);
        submission.accepted != 0
    }

    fn resolve_media_submissions(&mut self, handoffs: impl IntoIterator<Item = (Handoff, bool)>) {
        for (handoff, accepted) in handoffs {
            if let Some(participant) = self.registry.resolve_mut(handoff.participant)
                && participant.resolve_handoff(handoff.id, accepted)
            {
                self.timers.wake(handoff.participant);
            }
        }
    }

    fn authorize_media_handoff(
        runtime: &ShardRuntime,
        registry: &mut ParticipantRegistry,
        handoff: Handoff,
        unix_now: Option<u64>,
    ) -> bool {
        use crate::shard::router::FabricOwner;
        let allowed = match runtime.fabric_handoff_owner(handoff.branch, handoff.participant) {
            Some(FabricOwner::Local(publisher)) if registry.is_quic_owner(publisher) => runtime
                .quic_media_association(handoff.branch.source(), publisher)
                .zip(unix_now)
                .is_some_and(|(association, now)| {
                    registry.quic_owner_authorized(publisher, association, now)
                }),
            Some(FabricOwner::Local(publisher)) => registry
                .resolve_mut(publisher)
                .is_some_and(|owner| owner.fabric_source_authorized(handoff.branch.source())),
            Some(FabricOwner::Shard(_) | FabricOwner::Network) => true,
            None => false,
        };
        allowed
            && runtime.fabric_ingress_live(handoff.branch, handoff.ingress)
            && registry
                .resolve_mut(handoff.participant)
                .is_some_and(|receiver| {
                    receiver.fabric_io_authorized()
                        && receiver.handoff_pending(handoff.id)
                        && runtime
                            .fabric_dd_source(handoff.branch.source())
                            .is_none_or(|(track, encoding)| {
                                runtime
                                    .fabric_branch_target(handoff.branch)
                                    .is_some_and(|target| {
                                        receiver.fabric_video_output_selects(
                                            handoff.branch,
                                            track,
                                            encoding,
                                            target,
                                        )
                                    })
                            })
                })
    }

    pub(super) fn update_live_relay(
        &mut self,
        change: crate::route::fabric::LiveRelayChange,
    ) -> bool {
        use crate::route::fabric::LiveRelayChange;
        match change {
            LiveRelayChange::Install(relay) => {
                self.runtime.install_live_relay(Instant::now(), relay)
            }
            LiveRelayChange::Retire(relay) => self.runtime.retire_live_relay(&relay),
        }
    }

    pub(super) fn install_live_network_child(
        &mut self,
        child: crate::route::fabric::LiveNetworkChild,
        lifetime: pulsebeam_link::BindingCutoff,
    ) -> Option<crate::route::fabric::LiveNetworkChild> {
        let track = self.runtime.track_handle(child.publication)?;
        let plan = self.plans.get(track)?;
        if !plan.local.iter().all(|member| {
            self.registry
                .resolve_mut(*member)
                .is_some_and(|receiver| receiver.fabric_io_authorized())
        }) {
            return None;
        }
        let installed = self
            .runtime
            .install_live_network_child(child, lifetime, plan)?;
        self.wake_installed_fabric_receivers(track, child.publication);
        Some(installed)
    }

    fn wake_installed_fabric_receivers(
        &mut self,
        track: crate::keys::TrackHandle,
        publication: crate::entity::PublicationKey,
    ) {
        if publication.kind() != crate::entity::TrackKind::Data {
            self.quic_data_ready |= self.plans.get(track).is_some_and(|plan| {
                plan.local
                    .iter()
                    .any(|&member| self.registry.is_quic_owner(member))
            });
            self.bind_installed_fabric_rtp_receivers(track, publication);
        } else if let Some(plan) = self.plans.get(track) {
            for member in &plan.local {
                if self.registry.is_quic_owner(*member) {
                    self.quic_data_ready = true;
                } else if let Some(receiver) = self.registry.resolve_mut(*member) {
                    receiver.wake_fabric_receiver(publication);
                    self.dirty.mark(*member, receiver);
                }
            }
        }
    }

    fn record_relay_data_outcome(&self, outcome: crate::route::fabric::local::PropagationOutcome) {
        use crate::route::fabric::local::PropagationOutcome;
        let outcome = match outcome {
            PropagationOutcome::Confirmed => "confirmed",
            PropagationOutcome::Failed => "failed",
            PropagationOutcome::Unknown => "unknown",
        };
        metrics::counter!("fabric_relay_data_expectation_outcome", "outcome" => outcome)
            .increment(1);
    }

    fn data_publisher_source(
        &self,
        publisher: crate::route::fabric::DataPublisher,
    ) -> Option<(ParticipantHandle, FabricSourceHandle)> {
        use crate::entity::TrackKind;
        if publisher.publication.kind() != TrackKind::Data {
            return None;
        }
        let participant = self.registry.resolve_connection(publisher.connection)?;
        let track = self.runtime.track_handle(publisher.publication)?;
        let source = self.runtime.fabric_source_for_track(track)?;
        if self.runtime.fabric_incarnation(source) != Some(publisher.incarnation)
            || self.runtime.local_fabric_publication(source, participant)
                != Some(publisher.publication)
        {
            return None;
        }
        Some((participant, source))
    }

    pub(super) fn observe_publisher_data_expectation(
        &mut self,
        observation: crate::route::fabric::PublisherObservation,
    ) {
        let Some((participant, source)) = observation
            .route
            .publisher
            .data_publisher()
            .and_then(|publisher| self.data_publisher_source(publisher))
        else {
            return;
        };
        let outcome = observation.outcome;
        if self.fabric_publisher_authorized(participant, source)
            && self
                .runtime
                .observe_publisher_data_expectation(source, observation, Instant::now())
        {
            self.record_relay_data_outcome(outcome);
        }
    }

    pub(super) fn sample_publisher_anchor(
        &mut self,
        publisher: crate::shard_update::PublisherAnchor,
        now: Instant,
    ) -> Option<crate::shard_update::PublisherPosition> {
        if let Some(data) = publisher.data_publisher() {
            return Some(crate::shard_update::PublisherPosition {
                sequence: self.data_publisher_anchor(data)?,
                timing: None,
            });
        }
        if publisher.emission.profile() != pulsebeam_core::fabric::Profile::Opus {
            return None;
        }
        let (participant, source) = self.rtp_publisher_source(publisher)?;
        if !self.fabric_publisher_authorized(participant, source) {
            return None;
        }
        self.runtime.fabric_sequence_anchor(source, now)
    }

    fn rtp_publisher_source(
        &self,
        publisher: crate::shard_update::PublisherAnchor,
    ) -> Option<(ParticipantHandle, FabricSourceHandle)> {
        if publisher.emission.profile() == pulsebeam_core::fabric::Profile::Data {
            return None;
        }
        let participant = self.registry.resolve_connection(publisher.connection)?;
        let source = self
            .runtime
            .fabric_source_for_track(self.runtime.track_handle(publisher.publication)?)?;
        if self.runtime.local_fabric_publication(source, participant) != Some(publisher.publication)
            || self.runtime.fabric_incarnation(source) != Some(publisher.incarnation)
            || self.runtime.fabric_emission(source) != Some(publisher.emission)
        {
            return None;
        }
        Some((participant, source))
    }

    fn data_publisher_anchor(
        &mut self,
        publisher: crate::route::fabric::DataPublisher,
    ) -> Option<u64> {
        let (participant, source) = self.data_publisher_source(publisher)?;
        let installation = self
            .registry
            .resolve_mut(participant)?
            .fabric_data_input_installation(source)?;
        self.runtime
            .data_publisher(source, participant, installation)?
            .0
            .sequence_anchor()
    }

    pub(super) fn update_publisher_send(
        &mut self,
        change: crate::route::fabric::PublisherSendChange,
    ) -> bool {
        use crate::route::fabric::PublisherSendChange;
        match change {
            PublisherSendChange::Install { route, lifetime } => {
                let identity = route.publisher;
                let now = Instant::now();
                let binding = match identity.emission.profile() {
                    pulsebeam_core::fabric::Profile::Data => identity
                        .data_publisher()
                        .and_then(|publisher| self.data_publisher_source(publisher)),
                    pulsebeam_core::fabric::Profile::Opus if cfg!(feature = "sim") => {
                        self.rtp_publisher_source(identity)
                    }
                    _ => None,
                };
                let Some((publisher, source)) = binding else {
                    return false;
                };
                #[cfg(feature = "sim")]
                let wan_data = (identity.emission.profile()
                    == pulsebeam_core::fabric::Profile::Data)
                    .then(|| (identity.publication.to_string(), lifetime.clone()));
                let installed = self.fabric_publisher_authorized(publisher, source)
                    && self
                        .runtime
                        .install_publisher_send(source, route, lifetime, now);
                #[cfg(feature = "sim")]
                if installed && let Some((publication, cutoff)) = wan_data {
                    crate::sim_metrics::record_wan_data_send(publication, cutoff);
                }
                installed
            }
            PublisherSendChange::Retire(route) => self.runtime.retire_publisher_send(&route),
        }
    }

    pub(super) fn accept_network_data_expectation(
        &mut self,
        notice: crate::route::fabric::NetworkDataExpectation,
    ) -> bool {
        let now = Instant::now();
        let registry = &mut self.registry;
        self.runtime
            .accept_network_data_expectation(notice, now, |branch, destination| {
                registry
                    .resolve_mut(destination)
                    .and_then(|owner| owner.reliable_subscriber_branch(branch))
                    .filter(|lease| lease.endpoint.source == notice.parent.incarnation)
                    .map(|lease| lease.endpoint.branch)
            })
    }

    fn authorized_data_publisher_source(
        &mut self,
        identity: crate::route::fabric::DataPublisher,
    ) -> Option<(ParticipantHandle, FabricSourceHandle)> {
        let (publisher, source) = self.data_publisher_source(identity)?;
        self.fabric_publisher_authorized(publisher, source)
            .then_some((publisher, source))
    }

    pub(super) fn begin_peer_repair_lookup(
        &mut self,
        request: crate::shard_update::LocateRelayRepair,
    ) {
        use crate::route::fabric::relay::PeerRepairLookup;
        if request.result.is_closed() {
            return;
        }
        let now = Instant::now();
        match request.lookup {
            PeerRepairLookup::Media(lookup) => {
                let located = self.locate_peer_media_repair(lookup, now);
                let _ = request.result.send(located);
            }
            PeerRepairLookup::Publisher(lookup) => {
                if self.pending_publisher_lookups.len() >= PUBLISHER_LOOKUP_LIMIT {
                    let _ = request.result.send(None);
                    return;
                }
                self.advance_publisher_lookup(
                    PendingPublisherLookup {
                        lookup,
                        result: request.result,
                        due: now,
                    },
                    now,
                );
            }
        }
    }

    fn advance_publisher_lookup(&mut self, mut pending: PendingPublisherLookup, now: Instant) {
        if pending.result.is_closed() {
            return;
        }
        match self.resolve_publisher_lookup(&pending.lookup, now) {
            Some(PublisherLookupResult::Located(located)) => {
                let _ = pending.result.send(Some(located));
            }
            Some(PublisherLookupResult::RetryAt(due)) => {
                pending.due = due;
                self.pending_publisher_lookups.push(pending);
            }
            None => {
                let _ = pending.result.send(None);
            }
        }
    }

    fn resolve_publisher_lookup(
        &mut self,
        lookup: &crate::route::fabric::relay::PublisherRepairLookup,
        now: Instant,
    ) -> Option<PublisherLookupResult> {
        use crate::{
            participant::data_fabric::OriginalResolution,
            route::fabric::relay::{LocatedRepair, PeerRepairLookup},
        };
        let (publisher, source) =
            self.authorized_data_publisher_source(lookup.route.publisher.data_publisher()?)?;
        if let Some(located) = self.runtime.locate_publisher_repair(source, lookup, now) {
            return Some(PublisherLookupResult::Located(located));
        }
        let (window, _) = self.runtime.publisher_repair_window(source, lookup, now)?;
        let installation = self.runtime.reliable_data_installation(source)?;
        let result = self.resolve_data_original(
            publisher,
            source,
            installation,
            lookup.request.fragment,
            now,
        );
        match result {
            OriginalResolution::Cached {
                sequence, expires, ..
            } => Some(PublisherLookupResult::Located(LocatedRepair {
                lookup: Box::new(PeerRepairLookup::Publisher(lookup.clone())),
                sequence,
                expires: expires.min(window),
            })),
            OriginalResolution::Pending | OriginalResolution::Request(_) => {
                let due = now
                    .checked_add(PUBLISHER_LOOKUP_RETRY)
                    .unwrap_or(window)
                    .min(window);
                Some(PublisherLookupResult::RetryAt(due))
            }
            OriginalResolution::Unavailable
            | OriginalResolution::Exhausted
            | OriginalResolution::Revoked => None,
        }
    }

    fn retry_publisher_lookups(&mut self, now: Instant) {
        for _ in 0..FABRIC_CONTROL_TURN_BUDGET {
            let Some(index) = self
                .pending_publisher_lookups
                .iter()
                .position(|pending| pending.due <= now || pending.result.is_closed())
            else {
                break;
            };
            let pending = self.pending_publisher_lookups.swap_remove(index);
            self.advance_publisher_lookup(pending, now);
        }
    }

    fn authorized_rtp_publisher_source(
        &mut self,
        publisher: crate::shard_update::PublisherAnchor,
    ) -> Option<(ParticipantHandle, FabricSourceHandle)> {
        let (participant, source) = self.rtp_publisher_source(publisher)?;
        self.fabric_publisher_authorized(participant, source)
            .then_some((participant, source))
    }

    fn locate_peer_media_repair(
        &mut self,
        lookup: crate::route::fabric::relay::MediaRepairLookup,
        now: Instant,
    ) -> Option<crate::route::fabric::relay::LocatedRepair> {
        use crate::route::fabric::relay::MediaRepairParent;
        let MediaRepairParent::Publisher(route) = &lookup.parent else {
            return self.runtime.locate_media_repair(lookup, None, now);
        };
        let (publisher, source) = self.authorized_rtp_publisher_source(route.publisher)?;
        if let Some(located) = self
            .runtime
            .locate_media_repair(lookup.clone(), Some(source), now)
        {
            return Some(located);
        }
        self.runtime
            .request_peer_media_refresh(source, route, lookup.position, now, |ssrc| {
                let Some(owner) = self.registry.resolve_mut(publisher) else {
                    return false;
                };
                if !owner.request_fabric_refresh(ssrc) {
                    return false;
                }
                self.dirty.mark(publisher, owner);
                true
            });
        None
    }

    pub(super) fn send_peer_repair(
        &mut self,
        handoff: crate::route::fabric::relay::PeerRepairHandoff,
    ) {
        use crate::route::fabric::relay::PeerRepairLookup;
        let now = Instant::now();
        let (publisher_source, original) = match handoff.located.lookup.as_ref() {
            PeerRepairLookup::Media(lookup) => {
                use crate::route::fabric::relay::MediaRepairParent;
                match &lookup.parent {
                    MediaRepairParent::Publisher(route) => {
                        let Some((_, source)) =
                            self.authorized_rtp_publisher_source(route.publisher)
                        else {
                            return;
                        };
                        (Some(source), None)
                    }
                    MediaRepairParent::Relay(_) => (None, None),
                }
            }
            PeerRepairLookup::Publisher(lookup) => {
                let Some((publisher, source)) = lookup
                    .route
                    .publisher
                    .data_publisher()
                    .and_then(|publisher| self.authorized_data_publisher_source(publisher))
                else {
                    return;
                };
                let cached = (|| {
                    let installation = self.runtime.reliable_data_installation(source)?;
                    self.registry
                        .resolve_mut(publisher)?
                        .reliable_publisher_lease(source, installation)?;
                    let (data, fanout) =
                        self.runtime
                            .data_publisher(source, publisher, installation)?;
                    let fragment = lookup.request.fragment;
                    let (sequence, packet, expires) =
                        data.cached_original(fanout, fragment.unit_seq, fragment.index, now)?;
                    (sequence == handoff.located.sequence).then_some((packet, expires))
                })();
                (Some(source), cached)
            }
        };
        self.runtime
            .send_peer_repair(now, handoff, publisher_source, original);
    }

    pub(super) fn settle_unavailable_network_repair(
        &mut self,
        interest: crate::route::fabric::network_repair::Interest,
    ) {
        let Some(receiver) = self.registry.resolve_connection(interest.receiver) else {
            return;
        };
        let Some(branch) = self
            .runtime
            .network_fabric_branch(interest.parent.destination, receiver)
        else {
            return;
        };
        let Some(lease) = self
            .registry
            .resolve_mut(receiver)
            .and_then(|owner| owner.reliable_subscriber_branch(branch))
            .filter(|lease| {
                lease.endpoint.source == interest.parent.incarnation
                    && lease.endpoint.branch == interest.branch
            })
        else {
            return;
        };
        let target = branch.repair_target(receiver, lease.endpoint.branch);
        if self.runtime.settle_unavailable_network_repair(
            branch.source(),
            target,
            interest,
            Instant::now(),
        ) {
            self.report_fabric_repair(
                receiver,
                lease,
                interest.request,
                pulsebeam_proto::data_control::RepairStatus::Unavailable,
            );
        }
    }

    pub(super) fn install_media_repair_child(
        &mut self,
        child: crate::route::fabric::network_repair::MediaRepairChild,
        lifetime: pulsebeam_link::BindingCutoff,
    ) -> Option<crate::route::fabric::network_repair::MediaRepairChild> {
        self.runtime
            .install_media_repair_child(child, lifetime, Instant::now())
            .then_some(child)
    }

    pub(super) fn install_repair_network_child(
        &mut self,
        child: crate::route::fabric::network_repair::RepairChild,
        lifetime: pulsebeam_link::BindingCutoff,
    ) -> Option<crate::route::fabric::network_repair::RepairChild> {
        let receiver = self.registry.resolve_connection(child.receiver)?;
        let branch = self
            .runtime
            .network_fabric_branch(child.destination, receiver)?;
        let lease = self
            .registry
            .resolve_mut(receiver)?
            .reliable_subscriber_branch(branch)?;
        if lease.endpoint.source != child.route.source
            || lease.endpoint.branch != child.route.branch
        {
            return None;
        }
        let target = branch.repair_target(receiver, lease.endpoint.branch);
        self.runtime
            .install_network_repair(child, target, lifetime, Instant::now())
    }

    pub(super) fn apply_lifecycle_op(&mut self, op: &crate::shard_update::ShardUpdateOp) {
        debug_assert!(op.is_owned_by(self.shard_id));
        match op {
            crate::shard_update::ShardUpdateOp::InstallRoute { address, action } => {
                let Some(handle) = self.runtime.track_handle(action.publication_id()) else {
                    debug_assert!(false, "a route must target a live shard track");
                    return;
                };
                let action = match action {
                    crate::shard_update::TrackRouteAction::Forward { .. } => {
                        crate::route::RouteAction::Forward { target: handle }
                    }
                    crate::shard_update::TrackRouteAction::Reverse { .. } => {
                        crate::route::RouteAction::Reverse { target: handle }
                    }
                };
                self.runtime.routes.install_action(*address, action);
            }
            crate::shard_update::ShardUpdateOp::InstallShardFabricSource {
                binding,
                encoding,
                emission,
            } => {
                if let Some(track) = self.runtime.track_handle(binding.publication)
                    && let Some(plan) = self.plans.get(track)
                    && self
                        .runtime
                        .install_shard_fabric_source(*binding, *encoding, *emission, plan)
                        .is_some()
                    && (emission.profile() == pulsebeam_core::fabric::Profile::Data
                        || Self::local_rtp_profile_enabled(emission.profile()))
                {
                    self.wake_installed_fabric_receivers(track, binding.publication);
                }
            }
            crate::shard_update::ShardUpdateOp::BindShardFabricDestination { binding } => {
                if let Some(track) = self.runtime.track_handle(binding.publication)
                    && let Some(plan) = self.plans.get(track)
                {
                    let _ = self.runtime.bind_shard_fabric_destination(*binding, plan);
                }
            }
            crate::shard_update::ShardUpdateOp::SetShardDataReceiver { ready, enabled } => {
                self.runtime.set_shard_data_ready(*ready, *enabled);
            }
            crate::shard_update::ShardUpdateOp::RetireRoute { address } => {
                let _ = self.runtime.routes.retire(*address);
                self.runtime.retire_fabric_route(*address);
            }
            crate::shard_update::ShardUpdateOp::RetireTransport { address } => {
                self.registry.retire_transport(*address);
            }
            crate::shard_update::ShardUpdateOp::Placeholder => {}
            crate::shard_update::ShardUpdateOp::RemoveParticipant {
                connection,
                address,
            } => {
                self.pending_replacements.retain(|(transport, config)| {
                    *transport != *address || config.connection_id != *connection
                });
                if let Some(handle) = self.registry.resolve_connection(*connection)
                    && self
                        .registry
                        .resolve_mut(handle)
                        .is_some_and(|meta| meta.ingress == *address)
                {
                    self.timers.cancel(handle);
                    self.runtime.retire_fabric_participant(handle);
                }
                let removed = self.registry.remove(*connection, *address);
                if let Some(removed) = removed
                    && let Some(index) = self.pending_replacements.iter().position(|(_, config)| {
                        removed.scope.room.is_some()
                            && config.scope == removed.scope
                            && config.participant_id == removed.participant_id
                    })
                    && let Some((transport, config)) = self.pending_replacements.remove(index)
                {
                    // The retiring generation has already replaced its plans. Only
                    // now may a new incarnation occupy the participant identity.
                    let _ = self.add_participant(transport, config);
                }
            }
            crate::shard_update::ShardUpdateOp::InsertTrackRuntime {
                publication_id,
                runtime,
            } => {
                let descriptor = runtime.descriptor.as_ref();
                let origin = descriptor
                    .and_then(|descriptor| self.registry.resolve_connection(descriptor.origin));
                let publisher = runtime
                    .publisher
                    .and_then(|publisher| self.registry.resolve_connection(publisher));
                let track_handle =
                    self.runtime
                        .install_track(*publication_id, descriptor, origin, publisher);
                if let (Some(publisher), Some(effect)) =
                    (runtime.publisher, runtime.publisher_effect.as_ref())
                {
                    let Some(handle) = self.registry.resolve_connection(publisher) else {
                        debug_assert!(
                            false,
                            "a published topic must be live shard={} connection={publisher}",
                            self.shard_id
                        );
                        return;
                    };
                    let Some(meta) = self.registry.resolve_mut(handle) else {
                        debug_assert!(false, "a resolved participant handle must be live");
                        return;
                    };
                    meta.apply(effect.clone(), Some(track_handle));
                }
            }
            crate::shard_update::ShardUpdateOp::RemoveTrackRuntime { publication_id } => {
                debug_assert!(
                    self.runtime
                        .track_handle(*publication_id)
                        .is_none_or(|handle| !self.plans.contains_key(handle)),
                    "track plan removal must precede runtime retirement"
                );
                self.runtime.retire_track(*publication_id);
            }
        }
    }

    pub(super) fn apply_participant_effect(
        &mut self,
        connection: crate::entity::ConnectionId,
        effect: crate::participant::ParticipantEffect,
    ) -> bool {
        let publication_id = effect.publication_id();
        let track_handle =
            publication_id.and_then(|publication_id| self.runtime.track_handle(publication_id));
        if publication_id.is_some() && track_handle.is_none() {
            return false;
        }
        let Some(handle) = self.registry.resolve_connection(connection) else {
            return false;
        };
        let Some(meta) = self.registry.resolve_mut(handle) else {
            return false;
        };
        let fabric_rtp =
            if let crate::participant::ParticipantEffect::TrackPublished { publication_id } =
                &effect
                && let Some(track) = track_handle
            {
                Some((*publication_id, track))
            } else {
                None
            };
        meta.apply(effect, track_handle);
        let ingress = fabric_rtp.and_then(|(publication, track)| {
            meta.prepare_local_rtp_ingress(publication, track)
                .or_else(|| meta.prepare_whip_rtp_ingress(publication, track))
                .map(|prepared| (track, prepared))
        });
        let simulcast = fabric_rtp.and_then(|(publication, track)| {
            meta.prepare_dd_fabric_ingress(publication, track)
                .map(|prepared| (track, prepared))
        });
        self.dirty.mark(handle, meta);
        if let Some((track, (meta, encodings))) = simulcast {
            let expected = encodings.len();
            let mut installed =
                arrayvec::ArrayVec::<_, { crate::track::MAX_SIMULCAST_LAYERS }>::new();
            for ingress in encodings {
                let Some((encoding, emission)) = ingress.source_encoding().zip(ingress.emission())
                else {
                    break;
                };
                let Some(source) = self.install_local_rtp_ingress(track, handle, ingress, emission)
                else {
                    break;
                };
                let Some(incarnation) = self.runtime.fabric_incarnation(source) else {
                    if let Some(owner) = self.registry.resolve_mut(handle) {
                        owner.detach_fabric_media_source(source);
                    }
                    self.runtime.retire_fabric_source(source);
                    break;
                };
                installed.push((source, encoding, emission, incarnation));
            }
            if installed.len() == expected {
                for (_, encoding, emission, incarnation) in installed {
                    self.pipeline
                        .push_shard_event(ShardEvent::LocalFabricRtpSourceReady {
                            meta: meta.clone(),
                            encoding,
                            incarnation,
                            emission,
                        });
                }
                self.bind_installed_fabric_rtp_receivers(track, meta.id);
            } else {
                for (source, _, _, _) in installed {
                    if let Some(owner) = self.registry.resolve_mut(handle) {
                        owner.detach_fabric_media_source(source);
                    }
                    self.runtime.retire_fabric_source(source);
                }
            }
        }
        if let Some((track, (meta, ingress))) = ingress {
            if let Some(emission) = ingress.emission()
                && let Some(encoding) = ingress.source_encoding()
                && let Some(source) =
                    self.install_local_rtp_ingress(track, handle, ingress, emission)
                && let Some(incarnation) = self.runtime.fabric_incarnation(source)
            {
                let publication = meta.id;
                self.pipeline
                    .push_shard_event(ShardEvent::LocalFabricRtpSourceReady {
                        meta,
                        encoding,
                        incarnation,
                        emission,
                    });
                self.bind_installed_fabric_rtp_receivers(track, publication);
            }
        }
        true
    }

    pub(super) fn apply_plan(&mut self, operation: &crate::shard_update::TrackPlanUpdate) -> usize {
        let Some(track_handle) = self.runtime.track_handle(operation.publication_id) else {
            // The QUIC owner fences a lost association immediately. A plan
            // queued before the controller observes that cutoff is stale.
            return 0;
        };
        let previous = self
            .plans
            .get(track_handle)
            .is_some_and(|plan| !plan.local.is_empty() || !plan.remote.is_empty());
        let cost = match &operation.plan {
            Some(plan) => {
                debug_assert!(plan.is_valid());
                debug_assert_eq!(
                    plan.remote.len(),
                    plan.compiled_remote.len(),
                    "remote plans must be compiled by the controller"
                );
                debug_assert_eq!(
                    plan.reverse_route.is_some(),
                    plan.compiled_reverse.is_some(),
                    "reverse plans must be compiled by the controller"
                );
                let local: Vec<_> = plan
                    .local
                    .iter()
                    .filter_map(|connection| {
                        let handle = self.registry.resolve_connection(*connection);
                        debug_assert!(handle.is_some(), "a track plan must name a live connection");
                        handle
                    })
                    .collect();
                self.quic_data_ready |= local
                    .iter()
                    .any(|&handle| self.registry.is_quic_owner(handle));
                self.plans.insert(
                    track_handle,
                    crate::shard::router::InstalledTrackPlan {
                        local,
                        remote: plan.compiled_remote.clone(),
                        wan_transit: plan.wan_transit,
                        reverse_route: plan
                            .compiled_network_reverse
                            .as_ref()
                            .or(plan.compiled_reverse.as_ref())
                            .cloned(),
                    },
                );
                plan.local
                    .len()
                    .saturating_add(plan.compiled_remote.len())
                    .saturating_add(usize::from(
                        plan.compiled_network_reverse.is_some() || plan.compiled_reverse.is_some(),
                    ))
            }
            None => {
                debug_assert!(self.plans.contains_key(track_handle));
                usize::from(self.plans.remove(track_handle).is_some())
            }
        };
        let installed = self.plans.get(track_handle);
        self.runtime.reconcile_fabric_plan(track_handle, installed);
        let active =
            installed.is_some_and(|plan| !plan.local.is_empty() || !plan.remote.is_empty());
        if previous != active
            && let Some(publisher) = self.runtime.source_origin(track_handle)
            && let Some(meta) = self.registry.resolve_mut(publisher)
        {
            meta.native_rtp_demand(operation.publication_id, active);
            self.dirty.mark(publisher, meta);
        }
        self.bind_installed_fabric_rtp_receivers(track_handle, operation.publication_id);
        cost
    }

    fn bind_installed_fabric_rtp_receivers(
        &mut self,
        track: crate::keys::TrackHandle,
        publication: crate::entity::PublicationKey,
    ) {
        let whole_profile = self
            .runtime
            .fabric_source_for_track(track)
            .and_then(|source| self.runtime.fabric_emission(source))
            .filter(|emission| !emission.selects_dd_encoding())
            .map(Emission::profile);
        let Some(plan) = self.plans.get(track) else {
            return;
        };
        for destination in plan.local.clone() {
            let Some(receiver) = self.registry.resolve_mut(destination) else {
                continue;
            };
            if let Some(profile) = whole_profile
                && let Some((mid, primary)) = receiver.whep_rtp_output(publication, profile)
            {
                self.bind_ready_fabric_rtp(destination, track, mid, primary, profile);
                continue;
            }
            let bindings = receiver.ready_rtp_bindings_for(track).collect::<Vec<_>>();
            for (mid, primary, negotiated) in bindings {
                self.bind_ready_fabric_rtp(destination, track, mid, primary, negotiated);
            }
        }
    }

    fn install_local_rtp_ingress(
        &mut self,
        track: crate::keys::TrackHandle,
        publisher: ParticipantHandle,
        ingress: crate::rtp::fabric_ingress::RtpIngressShadow,
        emission: Emission,
    ) -> Option<FabricSourceHandle> {
        let refresh = if matches!(
            emission.profile(),
            pulsebeam_core::fabric::Profile::H264 | pulsebeam_core::fabric::Profile::Av1
        ) {
            std::num::NonZeroU32::new(ingress.primary_ssrc())
        } else {
            None
        };
        if !self.fabric_io_authorized(publisher) {
            return None;
        }
        let encoding = ingress.source_encoding()?;
        let source = self.runtime.install_fabric_source(
            track,
            publisher,
            emission,
            refresh,
            Some(encoding),
        )?;
        if !self.attach_fabric_ingress(source, publisher, ingress) {
            self.runtime.retire_fabric_source(source);
            return None;
        }
        Some(source)
    }

    pub(super) fn install_fabric_source(
        &mut self,
        track: crate::keys::TrackHandle,
        publisher: ParticipantHandle,
        emission: Emission,
        refresh_ssrc: Option<std::num::NonZeroU32>,
    ) -> Option<FabricSourceHandle> {
        if !self.fabric_io_authorized(publisher) {
            return None;
        }
        self.runtime
            .install_fabric_source(track, publisher, emission, refresh_ssrc, None)
    }

    pub(super) fn install_fabric_branch(
        &mut self,
        track: crate::keys::TrackHandle,
        source: FabricSourceHandle,
        destination: ParticipantHandle,
        receiver: Emission,
    ) -> Option<FabricLocalBranch> {
        if !self.fabric_io_authorized(destination) {
            return None;
        }
        self.runtime.install_fabric_branch(
            track,
            source,
            destination,
            receiver,
            self.plans.get(track)?,
            None,
        )
    }

    /// Install an explicitly qualified filtered candidate. The ordinary RTP
    /// writer cannot drain it before DD output bootstrap is implemented.
    pub(super) fn install_fabric_filtered_branch(
        &mut self,
        track: crate::keys::TrackHandle,
        source: FabricSourceHandle,
        destination: ParticipantHandle,
        receiver: Emission,
        target: pulsebeam_core::dd::evidence::DecodeTarget,
        now: Instant,
    ) -> Option<FabricLocalBranch> {
        if !self.fabric_io_authorized(destination) {
            return None;
        }
        let branch = self.runtime.install_fabric_branch(
            track,
            source,
            destination,
            receiver,
            self.plans.get(track)?,
            Some(target),
        )?;
        self.request_fabric_refresh(destination, branch, now);
        Some(branch)
    }

    pub(super) fn attach_fabric_ingress(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        ingress: crate::rtp::fabric_ingress::RtpIngressShadow,
    ) -> bool {
        self.attach_fabric_ingress_at(source, publisher, ingress, Instant::now())
    }

    fn attach_fabric_ingress_at(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        mut ingress: crate::rtp::fabric_ingress::RtpIngressShadow,
        now: Instant,
    ) -> bool {
        let Some(publication) = self.runtime.local_fabric_publication(source, publisher) else {
            return false;
        };
        let Some(identity) = ingress.take_identity() else {
            return false;
        };
        let reference = self
            .global_clock
            .as_mut()
            .and_then(|clock| clock.now(now))
            .map(|estimate| (now, estimate));
        let primary = ingress.primary_ssrc();
        let Some(mid) = ingress.mid() else {
            return false;
        };
        let Some(owner) = self.registry.resolve_mut(publisher) else {
            return false;
        };
        if !owner.attach_fabric_media_source(publication, ingress, source) {
            return false;
        }
        let report = owner.claim_fabric_sender_report(primary, mid);
        if !self
            .runtime
            .bind_fabric_media_identity(source, publisher, identity, primary, reference, report)
        {
            owner.detach_fabric_media_source(source);
            return false;
        }
        true
    }

    /// Stage the source and its exact RTC installation together. Callers must
    /// await the negotiated channel open before acknowledging the publication.
    pub(super) fn stage_fabric_data_publisher(
        &mut self,
        track: crate::keys::TrackHandle,
        publisher: ParticipantHandle,
        prepared: crate::participant::data_fabric::PreparedSctp,
        clock: Option<pulsebeam_proto::data_record::Clock<'_>>,
    ) -> Option<(FabricSourceHandle, std::num::NonZeroU64)> {
        use pulsebeam_proto::{data_control::PublisherBinding, data_endpoint::Endpoint};

        let source = self.install_fabric_source(track, publisher, Emission::DATA, None)?;
        let Some(incarnation) = self.runtime.fabric_incarnation(source) else {
            self.runtime.retire_fabric_source(source);
            return None;
        };
        let binding = prepared.bind(Endpoint::Publisher(PublisherBinding {
            source: incarnation,
        }));
        if !self.attach_fabric_data_ingress(source, publisher, binding, clock) {
            self.runtime.retire_fabric_source(source);
            return None;
        }
        Some((source, incarnation))
    }

    fn observe_fabric_subscriber(
        &mut self,
        destination: ParticipantHandle,
        feedback: crate::participant::event::FabricSubscriberFeedback,
        now: Instant,
        quic_connection: Option<crate::entity::ConnectionId>,
    ) {
        use crate::{
            participant::event::FabricFeedbackKind, route::fabric::local::ReceiptAcceptance,
        };
        let branch = feedback.lease.branch;
        let Some(owner) = self.runtime.fabric_handoff_owner(branch, destination) else {
            return;
        };
        if let crate::shard::router::FabricOwner::Local(publisher) = owner
            && !self.fabric_publisher_authorized(publisher, branch.source())
        {
            return;
        }
        let connection = if let Some(connection) = quic_connection {
            if self
                .registry
                .reliable_receiver(destination, branch, self.fabric_unix_time())
                != Some(feedback.lease)
            {
                return;
            }
            connection
        } else {
            let Some(connection) = self.registry.resolve_mut(destination).and_then(|receiver| {
                (receiver.reliable_subscriber_lease(feedback.lease.owned) == Some(feedback.lease))
                    .then_some(receiver.connection_id)
            }) else {
                return;
            };
            connection
        };
        match feedback.kind {
            FabricFeedbackKind::Propagation(unit, outcome) => {
                self.runtime.settle_fabric_data_propagation(
                    destination,
                    branch,
                    unit,
                    outcome,
                    now,
                );
            }
            FabricFeedbackKind::Assembled(unit) => {
                let Some(receipt) = self.runtime.record_fabric_assembly_receipt(
                    destination,
                    feedback.lease,
                    unit,
                    now,
                ) else {
                    return;
                };
                if receipt == ReceiptAcceptance::First {
                    metrics::counter!("fabric_data_assembly_receipts").increment(1);
                }
                if quic_connection.is_some() {
                    use pulsebeam_proto::data_control::subscriber_observation::Record;
                    self.quic_data_ready |= self.registry.enqueue_quic_subscriber_control(
                        destination,
                        feedback.lease,
                        Record::ReceiptAck(feedback.lease.expectation(unit)),
                    );
                } else if let Some(owner) = self.registry.resolve_mut(destination)
                    && owner.write_fabric_receipt_ack(feedback.lease, unit)
                {
                    #[cfg(feature = "sim")]
                    crate::sim_metrics::record_wan_data_receipt_ack(
                        feedback.lease.endpoint.source.get(),
                        unit.unit_seq,
                    );
                    self.dirty.mark(destination, owner);
                }
            }
            FabricFeedbackKind::Repair(request) => {
                use crate::route::fabric::local::RepairAdmission;
                use pulsebeam_proto::data_control::RepairStatus;
                let target = branch.repair_target(destination, feedback.lease.endpoint.branch);
                let admission = if owner == crate::shard::router::FabricOwner::Network {
                    match self.runtime.request_network_repair(
                        branch.source(),
                        target,
                        connection,
                        request,
                        now,
                    ) {
                        Ok(interest) => {
                            self.pipeline.push_network_data_repair(
                                crate::route::fabric::network_repair::InterestEvent::Request(
                                    interest,
                                ),
                            );
                            return;
                        }
                        Err(admission) => admission,
                    }
                } else {
                    self.runtime
                        .repair_fanout(branch.source())
                        .map_or(RepairAdmission::Ineligible, |fanout| {
                            fanout.request_data_repair(now, target, request)
                        })
                };
                let status = match admission {
                    RepairAdmission::Ineligible => RepairStatus::Unavailable,
                    RepairAdmission::Exhausted => RepairStatus::Exhausted,
                    RepairAdmission::Coalesced => RepairStatus::Unknown,
                    RepairAdmission::First | RepairAdmission::Replay => return,
                };
                self.report_fabric_repair(destination, feedback.lease, request, status);
            }
            FabricFeedbackKind::CancelRepair(request) => {
                let target = branch.repair_target(destination, feedback.lease.endpoint.branch);
                if owner == crate::shard::router::FabricOwner::Network {
                    if let Some(interest) = self.runtime.cancel_network_repair(
                        branch.source(),
                        target,
                        Some(connection),
                        request,
                        now,
                    ) {
                        self.pipeline.push_network_data_repair(
                            crate::route::fabric::network_repair::InterestEvent::Cancel(interest),
                        );
                    }
                } else if let Some(fanout) = self.runtime.repair_fanout(branch.source()) {
                    fanout.cancel_data_repair(target, request);
                }
            }
        }
    }

    fn report_fabric_repair(
        &mut self,
        destination: ParticipantHandle,
        lease: crate::participant::data_fabric::SubscriberLease,
        request: crate::route::fabric::local::RepairRequest,
        status: pulsebeam_proto::data_control::RepairStatus,
    ) {
        if self.registry.is_quic_owner(destination) {
            if self
                .registry
                .reliable_receiver(destination, lease.branch, self.fabric_unix_time())
                == Some(lease)
            {
                self.quic_data_ready |= self.registry.enqueue_quic_subscriber_control(
                    destination,
                    lease,
                    lease.repair_outcome(request, status),
                );
            }
        } else if let Some(receiver) = self.registry.resolve_mut(destination)
            && receiver.write_fabric_repair_outcome(lease, request, status)
        {
            self.dirty.mark(destination, receiver);
        }
    }

    fn complete_fabric_receiver_stage(
        &mut self,
        receiver: ParticipantHandle,
        work: crate::control::native::Preparation,
    ) {
        use pulsebeam_proto::data_endpoint::Endpoint;

        let prepared = self
            .registry
            .resolve_mut(receiver)
            .and_then(|owner| owner.prepared_native_fabric_receiver(work));
        let Some(prepared) = prepared else {
            return;
        };
        let Some(track) = self.runtime.track_handle(prepared.source) else {
            return;
        };
        let Some(source) = self.runtime.fabric_source_for_track(track) else {
            // Imported candidates can bind their SCTP channels before the peer receive route.
            return;
        };
        let branch =
            (|| {
                if self.runtime.fabric_incarnation(source) != Some(prepared.endpoint.source) {
                    return None;
                }
                let install_branch = if let Some(publisher) = self.runtime.source_origin(track) {
                    if !self.fabric_publisher_authorized(publisher, source) {
                        return None;
                    }
                    true
                } else {
                    self.runtime.is_shard_fabric_source(source)
                };
                let branch = if install_branch {
                    self.install_fabric_branch(track, source, receiver, Emission::DATA)?
                } else {
                    self.runtime.remote_fabric_branch(source, receiver)?
                };
                let binding = prepared
                    .prepared
                    .bind(Endpoint::Subscriber(prepared.endpoint));
                let clock = prepared.capture_clock.as_ref().map(|clock| {
                    pulsebeam_proto::data_record::Clock {
                        id: &clock.id,
                        epoch: clock.epoch,
                    }
                });
                if self.attach_fabric_data_egress(branch, receiver, binding, clock) {
                    Some(branch)
                } else {
                    self.runtime.retire_fabric_branch(branch, receiver);
                    None
                }
            })();
        let completed = if let Some(owner) = self.registry.resolve_mut(receiver) {
            let mut sink =
                self.pipeline
                    .participant_sink(crate::shard::events::ParticipantBinding {
                        participant_id: owner.participant_id,
                        connection_id: owner.connection_id,
                        handle: receiver,
                    });
            let completed = owner.complete_native_fabric_receiver(work, branch, &mut sink);
            self.dirty.mark(receiver, owner);
            completed
        } else {
            false
        };
        let Some(branch) = branch else {
            return;
        };
        if !completed {
            self.runtime.retire_fabric_branch(branch, receiver);
            return;
        }
        self.announce_shard_data_receiver(receiver, branch, work.reference);
    }

    fn announce_shard_data_receiver(
        &mut self,
        receiver: ParticipantHandle,
        branch: FabricLocalBranch,
        reference: u64,
    ) {
        if let Some(crate::shard::router::FabricOwner::Shard(binding)) =
            self.runtime.fabric_handoff_owner(branch, receiver)
            && let Some(connection) = self.registry.connection_id(receiver)
            && let Some(lease) =
                self.registry
                    .reliable_receiver(receiver, branch, self.fabric_unix_time())
            && lease.endpoint.source == binding.incarnation
        {
            self.pipeline
                .push_shard_event(ShardEvent::ShardDataReceiverReady(
                    crate::route::fabric::ShardDataReady {
                        binding,
                        reference,
                        receiver: crate::route::fabric::ShardDataReceiver {
                            connection,
                            branch: lease.endpoint.branch,
                        },
                    },
                ));
        }
    }

    fn complete_fabric_publisher_stage(
        &mut self,
        publisher: ParticipantHandle,
        work: crate::control::native::Preparation,
        publication: crate::entity::PublicationKey,
    ) {
        let prepared = self
            .registry
            .resolve_mut(publisher)
            .and_then(|owner| owner.prepared_native_fabric_publisher(work, publication))
            .map(|(prepared, clock)| (prepared, clock.cloned()));
        let mut reserved = false;
        let source = prepared.and_then(|(prepared, clock)| {
            if self.runtime.track_handle(publication).is_some() {
                return None;
            }
            let track = self
                .runtime
                .install_track(publication, None, Some(publisher), None);
            reserved = true;
            let clock = clock
                .as_ref()
                .map(|clock| pulsebeam_proto::data_record::Clock {
                    id: &clock.id,
                    epoch: clock.epoch,
                });
            self.stage_fabric_data_publisher(track, publisher, prepared, clock)
                .map(|(_, incarnation)| incarnation)
        });
        let completed = if let Some(owner) = self.registry.resolve_mut(publisher) {
            let mut sink =
                self.pipeline
                    .participant_sink(crate::shard::events::ParticipantBinding {
                        participant_id: owner.participant_id,
                        connection_id: owner.connection_id,
                        handle: publisher,
                    });
            let completed =
                owner.complete_native_fabric_publisher(work, publication, source, &mut sink);
            self.dirty.mark(publisher, owner);
            completed
        } else {
            false
        };
        if reserved && !completed {
            self.runtime.retire_track(publication);
        }
    }

    fn attach_fabric_data_ingress(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        binding: crate::participant::data_fabric::InstalledBinding,
        clock: Option<pulsebeam_proto::data_record::Clock<'_>>,
    ) -> bool {
        let pulsebeam_proto::data_endpoint::Endpoint::Publisher(endpoint) = binding.endpoint else {
            return false;
        };
        if !self
            .runtime
            .local_fabric_publication(source, publisher)
            .is_some_and(|publication| publication.kind() == crate::entity::TrackKind::Data)
            || self.runtime.fabric_incarnation(source) != Some(endpoint.source)
        {
            return false;
        }
        self.runtime
            .bind_data_publisher(source, publisher, binding, clock)
            && self
                .registry
                .resolve_mut(publisher)
                .is_some_and(|owner| owner.attach_fabric_data_source(source, binding))
    }

    fn resolve_data_original(
        &mut self,
        publisher: ParticipantHandle,
        source: FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
        fragment: crate::route::fabric::local::RepairFragment,
        now: Instant,
    ) -> crate::participant::data_fabric::OriginalResolution {
        use crate::participant::data_fabric::OriginalResolution;
        let Some(owner) = self.registry.resolve_mut(publisher) else {
            return OriginalResolution::Revoked;
        };
        let Some((owned, endpoint)) = owner.authorize_fabric_original_request(source, installation)
        else {
            return OriginalResolution::Revoked;
        };
        let Some((data, fanout)) = self
            .runtime
            .data_publisher_mut(source, publisher, installation)
        else {
            return OriginalResolution::Revoked;
        };
        let result = data.request_original(fanout, fragment.unit_seq, fragment.index, now);
        if let OriginalResolution::Request(request) = result {
            owner.send_fabric_original_request(owned, endpoint, request);
            self.dirty.mark(publisher, owner);
        }
        result
    }

    fn resolve_quic_data_original(
        &mut self,
        publisher: ParticipantHandle,
        source: FabricSourceHandle,
        fragment: crate::route::fabric::local::RepairFragment,
        now: Instant,
    ) -> crate::participant::data_fabric::OriginalResolution {
        use crate::participant::data_fabric::OriginalResolution;
        use pulsebeam_proto::data_control::publisher_observation::Record;
        let result = self
            .runtime
            .request_quic_data_original(source, publisher, fragment, now);
        if let OriginalResolution::Request(demand) = &result {
            self.quic_data_ready |= self.registry.enqueue_quic_publisher_control(
                publisher,
                source,
                Record::RequestOriginal(*demand),
            );
        }
        result
    }

    fn fabric_unix_time(&self) -> Option<u64> {
        self.wall
            .execution_time()
            .wall
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
    }

    fn fabric_publisher_authorized(
        &mut self,
        publisher: ParticipantHandle,
        source: FabricSourceHandle,
    ) -> bool {
        if self.registry.is_quic_owner(publisher) {
            return self
                .runtime
                .quic_data_association(source, publisher)
                .or_else(|| self.runtime.quic_media_association(source, publisher))
                .zip(self.fabric_unix_time())
                .is_some_and(|(association, now)| {
                    self.registry
                        .quic_owner_authorized(publisher, association, now)
                });
        }
        if !self.fabric_io_authorized(publisher) {
            self.runtime.retire_fabric_participant(publisher);
            return false;
        }
        let data = self
            .runtime
            .local_fabric_publication(source, publisher)
            .is_some_and(|publication| publication.kind() == crate::entity::TrackKind::Data);
        if !self.registry.resolve_mut(publisher).is_some_and(|owner| {
            if data {
                owner.fabric_data_source_authorized(source)
            } else {
                owner.fabric_source_authorized(source)
            }
        }) {
            self.runtime.fence_local_fabric_source(source, publisher);
            return false;
        }
        true
    }

    fn accept_reliable_data_admission(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        admission: crate::participant::data_fabric::ReliableAdmission,
        now: Instant,
    ) -> bool {
        if now >= admission.expires
            || !self.fabric_publisher_authorized(publisher, source)
            || self
                .registry
                .resolve_mut(publisher)
                .and_then(|owner| owner.reliable_publisher_lease(source, admission.installation))
                .is_none()
        {
            return false;
        }
        let unix_now = self.fabric_unix_time();
        let registry = &mut self.registry;
        let accepted = self.runtime.accept_local_data_expectation(
            source,
            publisher,
            admission,
            now,
            |branch, destination| {
                registry
                    .reliable_receiver(destination, branch, unix_now)
                    .map(|lease| lease.endpoint.branch)
            },
        );
        if !accepted {
            return false;
        }
        if admission.acknowledge
            && let Some(owner) = registry.resolve_mut(publisher)
            && owner.acknowledge_fabric_admission(source, admission)
        {
            self.dirty.mark(publisher, owner);
        }
        true
    }

    #[cfg(test)]
    pub(super) fn offer_fabric(
        &mut self,
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        now: Instant,
        arrival: Instant,
        sequence: u64,
        packet: OwnedPacket,
    ) -> Result<Offers, Rejection> {
        self.offer_fabric_publication(
            publisher,
            now,
            crate::participant::event::FabricPublication {
                source,
                arrival,
                sequence,
                packet,
                reliable: None,
            },
        )
    }

    #[cfg(test)]
    fn offer_fabric_publication(
        &mut self,
        publisher: ParticipantHandle,
        now: Instant,
        publication: crate::participant::event::FabricPublication,
    ) -> Result<Offers, Rejection> {
        let crate::participant::event::FabricPublication {
            source,
            arrival,
            sequence,
            packet,
            reliable,
        } = publication;
        if !self.fabric_publisher_authorized(publisher, source) {
            return Err(Rejection::Binding);
        }
        let data = packet.header().profile == pulsebeam_core::fabric::Profile::Data;
        let report = self
            .runtime
            .offer_fabric(source, publisher, now, arrival, sequence, packet)?;
        if !data {
            self.drain_fabric_rtp(source, now);
        }
        if data {
            self.drain_after_data_offer(now, source, sequence, reliable, &report);
        }
        self.refresh_after_fabric_offer(source, now, &report);
        Ok(report)
    }

    fn offer_fabric_data_position(
        &mut self,
        publisher: ParticipantHandle,
        now: Instant,
        publication: crate::participant::event::FabricDataPosition,
    ) {
        let source = publication.source;
        if !self.fabric_publisher_authorized(publisher, source)
            || self
                .registry
                .resolve_mut(publisher)
                .and_then(|owner| owner.fabric_data_input_installation(source))
                != Some(publication.installation)
            || self
                .runtime
                .data_publisher(source, publisher, publication.installation)
                .is_none_or(|(data, fanout)| {
                    data.sequence_anchor().is_none()
                        || fanout.endpoint_data(publication.sequence).is_none()
                })
        {
            return;
        }
        if let Some(admission) = publication.reliable
            && !self.accept_reliable_data_admission(source, publisher, admission, now)
        {
            return;
        }
        if let Ok(report) = self.runtime.offer_fabric_data(publisher, now, &publication) {
            self.drain_after_data_offer(
                now,
                source,
                publication.sequence,
                publication.reliable,
                &report,
            );
        }
    }

    fn drain_after_data_offer<I>(
        &mut self,
        now: Instant,
        source: FabricSourceHandle,
        sequence: u64,
        reliable: Option<crate::participant::data_fabric::ReliableAdmission<I>>,
        report: &Offers,
    ) {
        if report.enqueued != 0 || (reliable.is_some() && report.duplicates != 0) {
            if let Some(admission) = reliable
                && let Some(fanout) = self.runtime.repair_fanout(source)
            {
                fanout.claim_live_data(
                    now,
                    crate::route::fabric::local::RepairFragment {
                        unit_seq: admission.unit.unit_seq,
                        index: admission.fragment_index,
                    },
                    sequence,
                );
            }
            self.drain_fabric_data(source, now);
        }
    }

    fn refresh_after_fabric_offer(
        &mut self,
        source: FabricSourceHandle,
        now: Instant,
        report: &Offers,
    ) {
        if let Some((branch, destination)) = self.awaiting_fabric_refresh(source, report) {
            self.request_fabric_refresh(destination, branch, now);
        }
    }

    fn awaiting_fabric_refresh(
        &mut self,
        source: FabricSourceHandle,
        report: &Offers,
    ) -> Option<(FabricLocalBranch, ParticipantHandle)> {
        if report.awaiting_entry == 0 && report.suspended == 0 {
            return None;
        }
        let unix_now = self.fabric_unix_time();
        let registry = &mut self.registry;
        self.runtime.awaiting_fabric_refresh(source, |destination| {
            registry
                .resolve_mut(destination)
                .is_some_and(|owner| owner.fabric_io_authorized())
                || unix_now
                    .is_some_and(|now| registry.current_quic_owner_authorized(destination, now))
        })
    }

    fn send_shard_fabric_refresh(
        &mut self,
        destination: ParticipantHandle,
        branch: FabricLocalBranch,
        now: Instant,
        router: &impl IngressTransport,
    ) {
        if let Some(binding) = self
            .runtime
            .prepare_shard_fabric_refresh(branch, destination, now)
        {
            router.send_frame(binding.origin, ShardFrame::FabricRefresh(binding));
        }
    }

    pub(super) fn attach_fabric_rtp_egress(
        &mut self,
        branch: FabricLocalBranch,
        destination: ParticipantHandle,
        leg: crate::rtp::fabric_egress::SenderLeg<FabricLocalBranch>,
        source: crate::rtp::fabric_egress::SourceMetadata,
    ) -> bool {
        let Some(crate::shard::router::FabricOwner::Local(publisher)) =
            self.runtime.fabric_handoff_owner(branch, destination)
        else {
            return false;
        };
        self.fabric_publisher_authorized(publisher, branch.source())
            && self
                .registry
                .resolve_mut(destination)
                .is_some_and(|receiver| receiver.attach_fabric_rtp_output(branch, leg, source))
    }

    fn local_rtp_profile_enabled(profile: pulsebeam_core::fabric::Profile) -> bool {
        use pulsebeam_core::fabric::Profile;
        profile == Profile::Opus || profile.is_video()
    }

    fn stage_local_rtp_receiver(&mut self, destination: ParticipantHandle, reference: u64) {
        let Some(receiver) = self.registry.resolve_mut(destination) else {
            return;
        };
        let Some((track, mid, primary, profile)) = receiver.local_rtp_receiver(reference) else {
            return;
        };
        self.bind_ready_fabric_rtp(destination, track, mid, primary, profile);
    }

    fn bind_ready_fabric_rtp(
        &mut self,
        destination: ParticipantHandle,
        track: crate::keys::TrackHandle,
        mid: crate::media::Mid,
        primary: u32,
        profile: pulsebeam_core::fabric::Profile,
    ) {
        if !Self::local_rtp_profile_enabled(profile) {
            return;
        }
        let Some(receiver) = self.registry.resolve_mut(destination) else {
            return;
        };
        let whole = self
            .runtime
            .fabric_source_for_track(track)
            .filter(|&source| {
                self.runtime
                    .fabric_emission(source)
                    .is_some_and(|emission| !emission.selects_dd_encoding())
            });
        let selected = whole
            .is_none()
            .then(|| receiver.selected_fabric_video_encoding(mid, track))
            .flatten();
        let mut committed = None;
        if let Some((bound, active)) = receiver.fabric_rtp_output_branches(mid) {
            let live = |branch| {
                self.runtime
                    .fabric_handoff_owner(branch, destination)
                    .is_some()
            };
            committed = active.filter(|&branch| live(branch));
            let bound_live = live(bound);
            if let Some(branch) = active.filter(|branch| Some(*branch) != committed) {
                self.retire_bound_fabric_rtp(destination, branch);
            }
            if bound_live {
                let Some((bound_track, encoding)) = self.runtime.fabric_dd_source(bound.source())
                else {
                    return;
                };
                if track == bound_track
                    && selected.is_some_and(|(selected, target)| {
                        selected.matches(encoding.primary, encoding.rid)
                            && self.runtime.fabric_branch_target(bound) == Some(target)
                    })
                {
                    return;
                }
                // A new request can replace a staged branch, but it cannot replace
                // the sender's committed epoch until a qualified packet is accepted.
                if Some(bound) != committed {
                    self.retire_bound_fabric_rtp(destination, bound);
                }
            }
            if selected.is_none()
                && let Some(branch) = committed.take()
            {
                self.retire_bound_fabric_rtp(destination, branch);
            }
        }
        let source = if let Some((selected, _)) = selected {
            self.runtime.fabric_source_for_encoding(track, selected)
        } else {
            self.runtime.fabric_source_for_track(track)
        };
        let Some(source) = source else { return };
        let Some(emission) = self
            .runtime
            .fabric_emission(source)
            .filter(|emission| emission.profile() == profile)
        else {
            return;
        };
        let requires_dd = emission.selects_dd_encoding();
        if requires_dd && selected.is_none() {
            return;
        }
        let install_branch = self.runtime.source_origin(track).is_some()
            || self.runtime.is_shard_fabric_source(source);
        let reuse = committed.filter(|&branch| {
            branch.source() == source
                && self.runtime.fabric_branch_target(branch) == selected.map(|(_, target)| target)
        });
        let branch = if reuse.is_some() {
            reuse
        } else if install_branch && requires_dd {
            let Some(target) = selected
                .and_then(|(_, index)| pulsebeam_core::dd::evidence::DecodeTarget::new(index))
            else {
                return;
            };
            if !self.fabric_io_authorized(destination) {
                return;
            }
            let Some(plan) = self.plans.get(track) else {
                return;
            };
            self.runtime.install_fabric_branch(
                track,
                source,
                destination,
                emission,
                plan,
                Some(target),
            )
        } else if install_branch {
            self.install_fabric_branch(track, source, destination, emission)
        } else if self.fabric_io_authorized(destination)
            && self
                .plans
                .get(track)
                .is_some_and(|plan| plan.local.contains(&destination))
        {
            self.runtime.remote_fabric_branch(source, destination)
        } else {
            None
        };
        let Some(branch) = branch else { return };
        if !self
            .registry
            .resolve_mut(destination)
            .is_some_and(|owner| owner.bind_local_rtp_output(branch, mid, primary, emission))
        {
            if install_branch && reuse.is_none() {
                self.runtime.retire_fabric_branch(branch, destination);
            }
        } else if matches!(
            profile,
            pulsebeam_core::fabric::Profile::H264 | pulsebeam_core::fabric::Profile::Av1
        ) {
            self.pipeline.request_fabric_refresh(destination, branch);
        }
    }

    fn retire_bound_fabric_rtp(
        &mut self,
        destination: ParticipantHandle,
        branch: FabricLocalBranch,
    ) {
        self.runtime.retire_fabric_branch(branch, destination);
        if let Some(receiver) = self.registry.resolve_mut(destination) {
            receiver.retire_fabric_rtp_scope(branch);
            self.dirty.mark(destination, receiver);
        }
    }

    fn drain_fabric_rtp(&mut self, source: FabricSourceHandle, now: Instant) {
        let branches: [_; crate::route::fabric::local::MAX_BRANCHES] = {
            let mut iter = self.runtime.fabric_destinations(source);
            std::array::from_fn(|_| iter.next())
        };
        let video_source = self.runtime.fabric_dd_source(source);
        for (branch, destination) in branches.into_iter().flatten() {
            if self.registry.is_quic_owner(destination) {
                self.quic_data_ready = true;
                continue;
            }
            if !self
                .registry
                .resolve_mut(destination)
                .is_some_and(|receiver| {
                    receiver.has_fabric_rtp_output(branch)
                        && video_source.is_none_or(|(track, encoding)| {
                            self.runtime
                                .fabric_branch_target(branch)
                                .is_some_and(|target| {
                                    receiver.fabric_video_output_selects(
                                        branch, track, encoding, target,
                                    )
                                })
                        })
                })
            {
                continue;
            }
            // One ordered DD offer can release a burst. Each successful
            // admission consumes one item from this bounded queue.
            loop {
                match self.admit_fabric(
                    branch,
                    destination,
                    now,
                    |receiver, sequence, packet, evidence| {
                        receiver
                            .queue_bound_fabric_rtp(
                                branch,
                                (sequence, &packet, evidence),
                                video_source,
                                now,
                            )
                            .map(|replaced| (packet.header().profile, replaced))
                    },
                ) {
                    Ok(Some((_profile, replaced))) => {
                        if let Some(branch) = replaced {
                            self.retire_bound_fabric_rtp(destination, branch);
                        }
                        if let Some(receiver) = self.registry.resolve_mut(destination) {
                            self.dirty.mark(destination, receiver);
                            #[cfg(feature = "sim")]
                            {
                                crate::sim_metrics::record_routing_counter("fabric_rtp_enqueued");
                                if receiver.has_whep_output() {
                                    crate::sim_metrics::record_routing_counter(
                                        "fabric_whep_rtp_enqueued",
                                    );
                                }
                                if self.runtime.is_shard_fabric_source(source) {
                                    crate::sim_metrics::record_routing_counter(
                                        "fabric_rtp_shard_enqueued",
                                    );
                                    use pulsebeam_core::fabric::Profile;
                                    match _profile {
                                        Profile::Opus => {
                                            crate::sim_metrics::record_routing_counter(
                                                "fabric_rtp_opus_shard_enqueued",
                                            );
                                        }
                                        Profile::H264 => {
                                            crate::sim_metrics::record_routing_counter(
                                                "fabric_rtp_h264_shard_enqueued",
                                            );
                                        }
                                        _ => {}
                                    }
                                } else if matches!(
                                    self.runtime.fabric_handoff_owner(branch, destination),
                                    Some(crate::shard::router::FabricOwner::Local(_))
                                ) {
                                    use pulsebeam_core::fabric::Profile;
                                    match _profile {
                                        Profile::Opus => {
                                            crate::sim_metrics::record_routing_counter(
                                                "fabric_rtp_opus_local_enqueued",
                                            );
                                        }
                                        Profile::H264 => {
                                            crate::sim_metrics::record_routing_counter(
                                                "fabric_rtp_h264_local_enqueued",
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(reason) => {
                        #[cfg(feature = "sim")]
                        if reason
                            == crate::rtp::fabric_egress::AdmissionError::Rtc(
                                pulsebeam_rtc::WriteError::RtpTooLarge,
                            )
                        {
                            crate::sim_metrics::record_routing_counter(
                                "fabric_rtp_egress_too_large",
                            );
                        }
                        tracing::debug!(?reason, "Could not enqueue local Fabric RTP");
                        break;
                    }
                }
            }
        }
    }

    pub(super) fn write_fabric_rtp(
        &mut self,
        branch: FabricLocalBranch,
        destination: ParticipantHandle,
        now: Instant,
        leg: &mut crate::rtp::fabric_egress::SenderLeg<FabricLocalBranch>,
        source: crate::rtp::fabric_egress::SourceMetadata,
    ) -> Result<Option<()>, crate::rtp::fabric_egress::AdmissionError> {
        let admitted = self.admit_fabric(
            branch,
            destination,
            now,
            |receiver, sequence, packet, evidence| {
                receiver.queue_fabric_rtp(leg, branch, source, (sequence, &packet, evidence), now)
            },
        )?;
        if admitted.is_some()
            && let Some(receiver) = self.registry.resolve_mut(destination)
        {
            self.dirty.mark(destination, receiver);
        }
        Ok(admitted)
    }

    pub(super) fn attach_fabric_data_egress(
        &mut self,
        branch: FabricLocalBranch,
        destination: ParticipantHandle,
        binding: crate::participant::data_fabric::InstalledBinding,
        clock: Option<pulsebeam_proto::data_record::Clock<'_>>,
    ) -> bool {
        let pulsebeam_proto::data_endpoint::Endpoint::Subscriber(endpoint) = binding.endpoint
        else {
            return false;
        };
        if self.runtime.fabric_incarnation(branch.source()) != Some(endpoint.source)
            || self
                .runtime
                .fabric_handoff_owner(branch, destination)
                .is_none()
        {
            return false;
        }
        let Some(output) =
            crate::participant::data_fabric::Output::from_subscriber(branch, binding, clock)
        else {
            return false;
        };
        self.registry
            .resolve_mut(destination)
            .is_some_and(|owner| owner.attach_fabric_data_output(output))
    }

    fn drain_fabric_data(&mut self, source: FabricSourceHandle, now: Instant) {
        let branches: [_; crate::route::fabric::local::MAX_BRANCHES] = {
            let mut iter = self.runtime.fabric_destinations(source);
            std::array::from_fn(|_| iter.next())
        };
        for (branch, destination) in branches.into_iter().flatten() {
            self.drain_fabric_data_branch(branch, destination, now);
        }
    }

    fn receive_network_repair(
        &mut self,
        address: crate::route::NodeRouteAddress,
        ingress: pulsebeam_link::FabricIngress,
        now: Instant,
        image: crate::route::fabric::NetworkPacket,
    ) {
        let Some((child, branch, destination)) =
            self.runtime.network_repair_target(address, ingress, now)
        else {
            return;
        };
        let Some(packet) = child.accept_image(image) else {
            return;
        };
        if self
            .registry
            .resolve_mut(destination)
            .is_some_and(|receiver| receiver.matches_network_repair(branch, child, &packet))
            && self.runtime.offer_network_repair(child, now, packet)
        {
            self.drain_fabric_data_branch(branch, destination, now);
        }
    }

    fn drain_fabric_data_branch(
        &mut self,
        branch: FabricLocalBranch,
        destination: ParticipantHandle,
        now: Instant,
    ) {
        use crate::shard::router::FabricOwner;
        let Some(owner) = self.runtime.fabric_handoff_owner(branch, destination) else {
            return;
        };
        if let FabricOwner::Local(publisher) = owner
            && !self.fabric_publisher_authorized(publisher, branch.source())
        {
            return;
        }
        if self.registry.is_quic_owner(destination) {
            self.quic_data_ready = true;
            return;
        }
        if !self.fabric_io_authorized(destination) {
            self.runtime.retire_fabric_participant(destination);
            return;
        }
        let Some(receiver) = self.registry.resolve_mut(destination) else {
            return;
        };
        let current_branch = receiver
            .reliable_subscriber_branch(branch)
            .and_then(|lease| {
                (self.runtime.fabric_incarnation(branch.source()) == Some(lease.endpoint.source))
                    .then_some(lease.endpoint.branch)
            });
        let Some(handoff) = self.runtime.admit_fabric_data(
            branch,
            destination,
            now,
            current_branch,
            |_, packet, repair| {
                #[cfg(feature = "sim")]
                let stale = matches!(owner, FabricOwner::Shard(_))
                    && crate::sim_metrics::is_stale_shard_data(
                        packet.payload(),
                        receiver.connection_id,
                    );
                let written = receiver
                    .write_fabric_data(branch, packet, repair.map(|repair| repair.fragment))
                    .is_ok();
                #[cfg(feature = "sim")]
                if written && stale {
                    crate::sim_metrics::record_stale_shard_data_write();
                }
                written
            },
        ) else {
            return;
        };
        if handoff.written {
            if let Some(request) = handoff.completed
                && let Some(lease) = receiver.reliable_subscriber_branch(branch)
            {
                receiver.write_fabric_repair_outcome(
                    lease,
                    request,
                    pulsebeam_proto::data_control::RepairStatus::HandedOff,
                );
            }
            self.dirty.mark(destination, receiver);
        } else {
            record_routing_drop("packet", "fabric_data", "local");
        }
    }

    fn admit_fabric<T, E>(
        &mut self,
        handle: FabricLocalBranch,
        destination: ParticipantHandle,
        now: Instant,
        write: impl FnOnce(
            &mut crate::participant::Connection,
            u64,
            PacketView<'_>,
            crate::route::fabric::local::OutputEvidence,
        ) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        use crate::shard::router::FabricOwner;
        let Some(owner) = self.runtime.fabric_handoff_owner(handle, destination) else {
            return Ok(None);
        };
        if let FabricOwner::Local(publisher) = owner
            && !self.fabric_publisher_authorized(publisher, handle.source())
        {
            return Ok(None);
        }
        if !self.fabric_io_authorized(destination) {
            self.runtime.retire_fabric_participant(destination);
            return Ok(None);
        }
        let Some(receiver) = self.registry.resolve_mut(destination) else {
            return Ok(None);
        };
        let clock = self.global_clock.as_ref();
        self.runtime
            .admit_fabric(handle, destination, now, |sequence, packet, evidence| {
                let capture = evidence.capture;
                let result = write(receiver, sequence, packet, evidence)?;
                if let Some(capture) = capture {
                    ShardCore::record_wan_capture_age(clock, capture, now, "handoff");
                }
                Ok(result)
            })
    }

    fn reset_fabric_output(&mut self, destination: ParticipantHandle, branch: FabricLocalBranch) {
        if self.runtime.lose_fabric_output_context(branch, destination)
            && let Some(scope) = branch.scope()
            && let Some(receiver) = self.registry.resolve_mut(destination)
            && receiver.discard_pending_fabric_rtp(scope)
        {
            self.dirty.mark(destination, receiver);
        }
    }

    fn request_fabric_refresh(
        &mut self,
        destination: ParticipantHandle,
        branch: FabricLocalBranch,
        now: Instant,
    ) {
        if !self.fabric_refresh_receiver_authorized(destination) {
            return;
        }
        let unix_now = self.fabric_unix_time();
        let registry = &mut self.registry;
        let dirty = &mut self.dirty;
        self.quic_data_ready |=
            self.runtime
                .request_fabric_refresh(branch, destination, now, |publisher, ssrc| {
                    Self::deliver_fabric_refresh(
                        registry,
                        dirty,
                        (now, unix_now),
                        branch.source(),
                        publisher,
                        ssrc,
                    )
                });
    }

    fn deliver_fabric_refresh(
        registry: &mut ParticipantRegistry,
        dirty: &mut DirtyTracker,
        (now, unix_now): (Instant, Option<u64>),
        source: FabricSourceHandle,
        publisher: ParticipantHandle,
        ssrc: u32,
    ) -> bool {
        if registry.is_quic_owner(publisher) {
            return unix_now
                .is_some_and(|at| registry.current_quic_owner_authorized(publisher, at))
                && registry.enqueue_quic_publisher_refresh(publisher, source, ssrc, now);
        }
        let Some(owner) = registry.resolve_mut(publisher) else {
            return false;
        };
        if !owner.fabric_source_authorized(source) || !owner.request_fabric_refresh(ssrc) {
            return false;
        }
        dirty.mark(publisher, owner);
        true
    }

    fn fabric_refresh_receiver_authorized(&mut self, participant: ParticipantHandle) -> bool {
        self.fabric_io_authorized(participant)
            || self.fabric_unix_time().is_some_and(|now| {
                self.registry
                    .current_quic_owner_authorized(participant, now)
            })
    }

    fn fabric_io_authorized(&mut self, participant: ParticipantHandle) -> bool {
        self.registry
            .resolve_mut(participant)
            .is_some_and(|p| p.fabric_io_authorized())
    }

    #[cfg(test)]
    pub(crate) fn on_udp_batch(&mut self, batch: net::RecvPacketBatch) {
        let Some(address) = self.registry.demux(&batch) else {
            return;
        };
        // Not an error: SO_REUSEPORT picks the receiving socket by hashing the
        // 4-tuple, which has nothing to do with which shard owns the route, so
        // a datagram for another shard arriving here is ordinary. Resolving a
        // route is not a claim to own it.
        if address.shard() != self.shard_id {
            metrics::counter!("shard_wrong_owner_drop").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_counter("shard_wrong_owner_drop");
            return;
        }
        self.on_owned_udp_batch(batch, address, self.shard_id);
    }

    pub(crate) fn on_udp_batch_routed(
        &mut self,
        batch: net::RecvPacketBatch,
        router: &impl IngressTransport,
    ) {
        let Some(address) = self.registry.demux(&batch) else {
            return;
        };
        if address.shard() != self.shard_id {
            router.send_frame(
                address.shard(),
                ShardFrame::Ingress {
                    batch,
                    address,
                    source_shard: self.shard_id,
                },
            );
            metrics::counter!("shard_wrong_owner_forward").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_counter("shard_wrong_owner_forward");
            return;
        }
        self.on_owned_udp_batch(batch, address, self.shard_id);
    }

    fn on_owned_udp_batch(
        &mut self,
        batch: net::RecvPacketBatch,
        address: NodeTransportAddress,
        source_shard: crate::id::ShardId,
    ) {
        debug_assert_eq!(address.shard(), self.shard_id);
        let Some(key) = self.registry.resolve_transport(address) else {
            return;
        };
        let Some(participant) = self.registry.resolve_mut(key) else {
            record_routing_drop("transport", "runtime", "local");
            return;
        };
        participant.input(crate::participant::ParticipantInput::Network {
            batch,
            source_shard,
        });
        self.dirty.mark(key, participant);
    }

    pub(crate) fn flush_stream_buffers(&mut self, now: Instant, budget: usize) -> usize {
        debug_assert!(budget > 0);
        use crate::shard::events::QueuedPublication;
        let reference = self.global_clock.as_mut().and_then(|clock| clock.now(now));
        let mut processed = 0;
        while processed < budget {
            let Some(queued) = self.pipeline.pop_publication() else {
                break;
            };
            processed = processed.saturating_add(1);
            match queued {
                QueuedPublication::FabricMedia {
                    publisher,
                    source,
                    input,
                } => {
                    if self.fabric_publisher_authorized(publisher, source) {
                        match self.runtime.offer_fabric_media(
                            source,
                            publisher,
                            now,
                            &input,
                            reference.as_ref(),
                        ) {
                            Ok(report) => {
                                use crate::route::fabric::local::DecodeTargetSample;
                                if (report.dd_structure.is_some()
                                    || report.dd_sample != DecodeTargetSample::None)
                                    && let Some((_, encoding)) =
                                        self.runtime.fabric_dd_source(source)
                                    && let Some(owner) = self.registry.resolve_mut(publisher)
                                {
                                    owner.measure_fabric_dd(
                                        source,
                                        encoding,
                                        now,
                                        report.dd_structure,
                                        report.dd_sample,
                                    );
                                }
                                self.drain_fabric_rtp(source, now);
                                self.refresh_after_fabric_offer(source, now, &report);
                            }
                            Err(Rejection::Conflict) => {
                                if let Some(owner) = self.registry.resolve_mut(publisher) {
                                    owner.fence_fabric_media(source);
                                }
                                self.runtime.fence_local_fabric_source(source, publisher);
                            }
                            Err(_) => (),
                        }
                    }
                    continue;
                }
                QueuedPublication::FabricData {
                    publisher,
                    publication,
                } => {
                    self.offer_fabric_data_position(publisher, now, publication);
                    continue;
                }
                #[cfg(test)]
                QueuedPublication::Fabric {
                    publisher,
                    publication,
                } => {
                    if let Some(admission) = publication.reliable
                        && !self.accept_reliable_data_admission(
                            publication.source,
                            publisher,
                            admission,
                            now,
                        )
                    {
                        continue;
                    }
                    let _ = self.offer_fabric_publication(publisher, now, publication);
                    continue;
                }
            }
        }
        processed
    }

    pub(crate) fn flush_participant_events(
        &mut self,
        router: &impl IngressTransport,
        budget: usize,
    ) -> usize {
        self.flush_participant_events_with_clock(router, budget, Instant::now)
    }

    fn flush_participant_events_with_clock(
        &mut self,
        router: &impl IngressTransport,
        budget: usize,
        now: impl Fn() -> Instant,
    ) -> usize {
        debug_assert!(budget > 0);
        let mut processed = 0;
        while processed < budget {
            let Some(event) = self.pipeline.pop_participant_event() else {
                break;
            };
            processed = processed.saturating_add(1);
            match event {
                ParticipantEvent::Binding(ev) => match ev {
                    ParticipantBindingEvent::Activated {
                        track,
                        subscriber,
                        connection_id,
                    } => self.pipeline.push_shard_event(ShardEvent::TrackSubscribed {
                        subscriber,
                        connection_id,
                        track,
                    }),
                    ParticipantBindingEvent::Deactivated {
                        track,
                        subscriber,
                        connection_id,
                    } => self
                        .pipeline
                        .push_shard_event(ShardEvent::TrackUnsubscribed {
                            subscriber,
                            connection_id,
                            track,
                        }),
                },
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Connected {
                    participant_key,
                    source,
                    destination,
                    source_shard,
                }) => {
                    let Some(address) = self.registry.authenticated_address(participant_key) else {
                        debug_assert!(false, "authenticated participant must still be registered");
                        continue;
                    };
                    self.registry.authenticate_addr(source, address);
                    self.pipeline
                        .push_shard_event(ShardEvent::TransportAuthenticated {
                            source,
                            destination,
                            source_shard,
                            address,
                            shard: self.shard_id,
                        });
                }
                ParticipantEvent::Lifecycle(ParticipantLifecycleEvent::Exited {
                    participant_id,
                    connection_id,
                }) => {
                    self.pipeline
                        .push_shard_event(ShardEvent::ParticipantClosed {
                            participant: participant_id,
                            connection_id,
                        });
                }
                ParticipantEvent::Control(ev) => {
                    self.pipeline.push_shard_event(ev);
                }
                ParticipantEvent::Internal(ev) => match ev {
                    crate::shard::events::ShardInternalEvent::FabricSubscriberFeedback {
                        destination,
                        feedback,
                    } => {
                        self.observe_fabric_subscriber(destination, feedback, now(), None);
                    }
                    crate::shard::events::ShardInternalEvent::StageLocalRtpReceiver {
                        receiver,
                        reference,
                    } => {
                        self.stage_local_rtp_receiver(receiver, reference);
                    }
                    crate::shard::events::ShardInternalEvent::StageFabricReceiver {
                        receiver,
                        work,
                    } => {
                        self.complete_fabric_receiver_stage(receiver, work);
                    }
                    crate::shard::events::ShardInternalEvent::StageFabricPublisher {
                        publisher,
                        work,
                        publication,
                    } => {
                        self.complete_fabric_publisher_stage(publisher, work, publication);
                    }
                    crate::shard::events::ShardInternalEvent::FenceFabricSource {
                        publisher,
                        source,
                    } => {
                        self.runtime.fence_local_fabric_source(source, publisher);
                    }
                    crate::shard::events::ShardInternalEvent::FabricRefresh {
                        destination,
                        branch,
                    } => {
                        if self.fabric_refresh_receiver_authorized(destination) {
                            if !self.registry.is_quic_owner(destination) {
                                self.reset_fabric_output(destination, branch);
                            }
                            match self.runtime.fabric_handoff_owner(branch, destination) {
                                Some(crate::shard::router::FabricOwner::Shard(_)) => {
                                    self.send_shard_fabric_refresh(
                                        destination,
                                        branch,
                                        now(),
                                        router,
                                    );
                                }
                                Some(crate::shard::router::FabricOwner::Local(_)) => {
                                    self.request_fabric_refresh(destination, branch, now());
                                }
                                _ => {}
                            }
                        }
                    }
                    crate::shard::events::ShardInternalEvent::ReverseRequested {
                        stream,
                        packet,
                    } => {
                        let Some(plan) = self.plans.get(stream) else {
                            debug_assert!(false, "reverse packet has no owned track plan");
                            continue;
                        };
                        self.runtime.route_reverse(packet, plan);
                    }
                },
            }
        }
        processed
    }

    pub(crate) fn has_pending_events(&self) -> bool {
        self.pipeline.has_pending()
    }

    pub(crate) fn pop_shard_event(&mut self) -> Option<ShardEvent> {
        self.pipeline.pop_shard_event()
    }

    pub(crate) fn on_command(
        &mut self,
        cmd: ShardCommand,
        router: &impl IngressTransport,
    ) -> Option<()> {
        match cmd {
            ShardCommand::MaterializeQuic { ack, .. } => {
                // QUIC materialization belongs to the shard worker's ingress owner.
                let _ = ack.send(false);
            }
            ShardCommand::MaterializeParticipant {
                transport,
                config,
                ack,
            } => {
                #[cfg(feature = "sim")]
                if crate::sim_metrics::take_materialization_failure() {
                    crate::sim_metrics::record_routing_counter("materialization_failed");
                    let _ = ack.send(false);
                    return Some(());
                }
                let endpoint = config.scope.endpoint(config.participant_id);
                let materialized = if endpoint
                    .and_then(|endpoint| self.registry.resolve_endpoint(endpoint))
                    .is_some()
                {
                    // Materialization and plan updates use independent mailboxes. The
                    // controller acknowledges this command before it can stage old
                    // incarnation cleanup, so defer insertion until that cleanup retires
                    // the old handle. Never let an old plan resolve to the new peer.
                    if self.pending_replacements.len() >= 256 {
                        tracing::warn!("pending participant replacement capacity exhausted");
                        false
                    } else {
                        self.pending_replacements.push_back((transport, *config));
                        true
                    }
                } else {
                    self.add_participant(transport, *config)
                };
                let _ = ack.send(materialized);
            }
            ShardCommand::AuthenticateTransport { source, address } => {
                self.registry.authenticate_addr(source, address);
                metrics::counter!("demux_flow_authenticated").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("demux_flow_authenticated");
            }
            ShardCommand::NativeReady { connection_id } => {
                if let Some(key) = self.registry.resolve_connection(connection_id)
                    && let Some(current) = self.registry.resolve_mut(key)
                {
                    current.native_ready();
                    self.dirty.mark(key, current);
                }
            }
            ShardCommand::PublicationCutoffResult {
                connection_id,
                reference,
                success,
            } => {
                if let Some(key) = self.registry.resolve_connection(connection_id)
                    && let Some(current) = self.registry.resolve_mut(key)
                {
                    current.input(crate::participant::ParticipantInput::PublicationCutoff {
                        reference,
                        success,
                    });
                    self.dirty.mark(key, current);
                }
            }
            ShardCommand::AuthorizationResult {
                connection_id,
                revision,
                expiry,
            } => {
                if let Some(key) = self.registry.resolve_connection(connection_id)
                    && let Some(current) = self.registry.resolve_mut(key)
                {
                    current.input(crate::participant::ParticipantInput::Authorization {
                        revision,
                        expiry,
                    });
                    self.dirty.mark(key, current);
                }
            }
        }
        let _ = router;
        Some(())
    }

    pub(crate) fn on_shard_frames(
        &mut self,
        frames: impl IntoIterator<Item = ShardFrame>,
        now: Instant,
        router: &impl IngressTransport,
    ) {
        for frame in frames {
            #[cfg(feature = "sim")]
            {
                if crate::sim_metrics::delay_shard_data_frame(&frame) {
                    continue;
                }
                if let Some(held) = crate::sim_metrics::take_released_shard_data(&frame, now) {
                    for delayed in held {
                        self.on_shard_frame(delayed, now, router);
                    }
                }
                if let Some(late) = crate::sim_metrics::take_expired_shard_data_receipt(&frame, now)
                {
                    self.on_shard_frame(late, now, router);
                }
            }
            self.on_shard_frame(frame, now, router);
        }
    }

    fn on_shard_frame(&mut self, frame: ShardFrame, now: Instant, router: &impl IngressTransport) {
        match frame {
            ShardFrame::Ingress {
                batch,
                address,
                source_shard,
            } => {
                // A datagram that reached the node on another shard's socket.
                // Ordinary while a flow is bootstrapping — the steering map is
                // a cache, and a miss lands on whatever the kernel's tuple hash
                // picked. A rate that does not fall once flows are established
                // means the map is not being populated.
                debug_assert_ne!(
                    source_shard, self.shard_id,
                    "a shard cannot forward to itself"
                );
                metrics::counter!("shard_ingress_forwarded").increment(1);
                #[cfg(feature = "sim")]
                crate::sim_metrics::record_routing_counter("shard_ingress_forwarded");
                if self.registry.resolve_transport(address).is_some() {
                    self.registry.learn_addr(batch.src, address);
                }
                self.on_owned_udp_batch(batch, address, source_shard);
            }
            ShardFrame::Fabric(offer) => {
                let binding = offer.binding;
                let data = offer.packet.header().profile == pulsebeam_core::fabric::Profile::Data;
                if let Ok(report) = self.runtime.offer_shard_fabric(now, offer)
                    && let Some(source) = self.runtime.shard_fabric_source(binding)
                {
                    if !data {
                        self.drain_fabric_rtp(source, now);
                        if let Some((branch, destination)) =
                            self.awaiting_fabric_refresh(source, &report)
                        {
                            self.send_shard_fabric_refresh(destination, branch, now, router);
                        }
                    } else if report.enqueued != 0 {
                        self.drain_fabric_data(source, now);
                    }
                }
            }
            ShardFrame::FabricDataExpectation(notice) => {
                let unix_now = self.fabric_unix_time();
                let registry = &self.registry;
                self.runtime
                    .accept_shard_data_expectation(&notice, now, |branch, destination| {
                        let connection = registry.connection_id(destination)?;
                        let lease = registry.reliable_receiver(destination, branch, unix_now)?;
                        (lease.endpoint.source == notice.binding.incarnation).then_some(
                            crate::route::fabric::ShardDataReceiver {
                                connection,
                                branch: lease.endpoint.branch,
                            },
                        )
                    });
            }
            ShardFrame::FabricDataRepair(repair) => self.on_shard_data_repair(repair, now),
            ShardFrame::FabricDataRepairImage(image) => self.on_shard_data_repair_image(image, now),
            ShardFrame::FabricDataReceipt(receipt) => {
                if let Some((source, publisher)) = self.runtime.shard_receipt_publisher(receipt)
                    && self.fabric_publisher_authorized(publisher, source)
                {
                    let _accepted = self.runtime.record_shard_data_receipt(source, now, receipt);
                    #[cfg(feature = "sim")]
                    match _accepted {
                        Some(crate::route::fabric::local::ReceiptAcceptance::First) => {
                            crate::sim_metrics::record_shard_data_receipt(receipt, now);
                        }
                        None => {
                            crate::sim_metrics::record_rejected_shard_data_receipt(receipt, now);
                        }
                        _ => {}
                    }
                }
            }
            ShardFrame::FabricRefresh(binding) => {
                let unix_now = self.fabric_unix_time();
                let registry = &mut self.registry;
                let dirty = &mut self.dirty;
                let delivered = self.runtime.request_shard_fabric_refresh(
                    binding,
                    now,
                    |source, publisher, ssrc| {
                        Self::deliver_fabric_refresh(
                            registry,
                            dirty,
                            (now, unix_now),
                            source,
                            publisher,
                            ssrc,
                        )
                    },
                );
                self.quic_data_ready |= delivered;
                #[cfg(feature = "sim")]
                if delivered {
                    crate::sim_metrics::record_routing_counter("fabric_rtp_shard_refresh");
                }
            }
            ShardFrame::QuicIngress { .. } | ShardFrame::FabricWanImage { .. } => {}
            ShardFrame::Reverse { address, packet } => self.on_reverse_frame(address, packet, now),
        }
    }

    fn on_shard_data_repair(
        &mut self,
        repair: crate::route::fabric::ShardDataRepair,
        now: Instant,
    ) {
        use crate::{
            participant::data_fabric::OriginalResolution, route::fabric::ShardDataRepairImage,
        };
        let Some((source, publisher, installation, route, count)) =
            self.runtime.resolve_shard_data_repair(now, repair)
        else {
            return;
        };
        let route = route.clone();
        if !self.fabric_publisher_authorized(publisher, source) {
            return;
        }
        let fragment = repair.request.fragment;
        let resolution = match installation {
            Some(installation) => {
                self.resolve_data_original(publisher, source, installation, fragment, now)
            }
            None => self.resolve_quic_data_original(publisher, source, fragment, now),
        };
        let OriginalResolution::Cached { sequence, expires } = resolution else {
            return;
        };
        let Some((position, packet, deadline)) =
            self.runtime.cached_data_original(source, fragment, now)
        else {
            return;
        };
        if position != sequence
            || now >= expires.min(deadline)
            || !pulsebeam_proto::data_record::wire_identity(packet.payload())
                .is_ok_and(|identity| identity.count == count)
        {
            return;
        }
        crate::shard::worker::send_link_frame(
            &route.tx,
            route.clock_origin,
            ShardFrame::FabricDataRepairImage(ShardDataRepairImage {
                repair,
                sequence,
                packet: packet.clone(),
            }),
        );
    }

    fn on_shard_data_repair_image(
        &mut self,
        image: crate::route::fabric::ShardDataRepairImage,
        now: Instant,
    ) {
        use crate::route::fabric::local::RepairQueue;
        let repair = image.repair;
        let Some(source) = self.runtime.shard_fabric_source(repair.binding) else {
            return;
        };
        let Some(receiver) = self.registry.resolve_connection(repair.receiver.connection) else {
            return;
        };
        let Some(branch) = self.runtime.remote_fabric_branch(source, receiver) else {
            return;
        };
        let Some(lease) =
            self.registry
                .reliable_receiver(receiver, branch, self.fabric_unix_time())
        else {
            return;
        };
        if lease.endpoint.source != repair.binding.incarnation
            || lease.endpoint.branch != repair.receiver.branch
        {
            return;
        }
        let target = branch.repair_target(receiver, lease.endpoint.branch);
        let Some(fanout) = self.runtime.repair_fanout(source) else {
            return;
        };
        #[cfg(feature = "sim")]
        let identity = pulsebeam_proto::data_record::wire_identity(image.packet.payload());
        let outcome = fanout.enqueue_shard_repair(now, target, image);
        #[cfg(feature = "sim")]
        if outcome == RepairQueue::Queued
            && let Ok(identity) = identity
        {
            crate::sim_metrics::record_shard_data_repair(identity);
        }
        if matches!(outcome, RepairQueue::Queued | RepairQueue::Backpressured) {
            self.drain_fabric_data_branch(branch, receiver, now);
        }
    }

    fn on_reverse_frame(
        &mut self,
        address: crate::route::NodeRouteAddress,
        packet: crate::participant::reverse::ReversePacket,
        now: Instant,
    ) {
        if !self
            .runtime
            .routes
            .accept_reverse(address, packet.dedup(), now)
        {
            return;
        }
        let Some(action) = self.runtime.routes.resolve(address) else {
            return;
        };
        let Some((origin, target)) = self.runtime.resolve_reverse(action) else {
            return;
        };
        if let Some(meta) = self.registry.resolve_mut(origin) {
            meta.input(crate::participant::ParticipantInput::Reverse {
                stream: target,
                packet,
            });
            self.dirty.mark(origin, meta);
        }
    }

    fn add_participant(
        &mut self,
        transport: crate::route::NodeTransportAddress,
        cfg: ConnectionConfig,
    ) -> bool {
        debug_assert_eq!(transport.shard(), self.shard_id);
        if let Some(previous) = cfg
            .scope
            .endpoint(cfg.participant_id)
            .and_then(|endpoint| self.registry.resolve_endpoint(endpoint))
        {
            self.timers.cancel(previous);
            self.runtime.retire_fabric_participant(previous);
        }
        let key = self.registry.insert(cfg, transport);
        if let Some(participant) = self.registry.resolve_mut(key) {
            self.dirty.mark(key, participant);
        }
        true
    }

    pub(crate) fn participant_count(&self) -> usize {
        self.registry.len()
    }

    pub(crate) fn next_timer_deadline(&mut self) -> Option<Instant> {
        self.timers
            .next_deadline()
            .into_iter()
            .chain(self.runtime.next_endpoint_data_expiry())
            .chain(
                self.runtime
                    .next_fabric_data_propagation()
                    .map(|(deadline, _, _)| deadline),
            )
            .chain(
                self.runtime
                    .next_shard_data_notice()
                    .map(|(deadline, _, _)| deadline),
            )
            .chain(
                self.runtime
                    .next_fabric_delivery_report()
                    .map(|(deadline, _, _)| deadline),
            )
            .chain(
                self.runtime
                    .next_fabric_data_repair()
                    .map(|(_, _, work)| work.due),
            )
            .chain(
                self.runtime
                    .next_network_repair_expiry()
                    .map(|(expires, ..)| expires),
            )
            .chain(
                self.pending_publisher_lookups
                    .iter()
                    .map(|lookup| lookup.due)
                    .min(),
            )
            .chain(
                self.runtime
                    .next_publisher_data_expectation()
                    .map(|(deadline, _)| deadline),
            )
            .min()
    }

    fn expire_network_repairs(&mut self, now: Instant) {
        for _ in 0..FABRIC_CONTROL_TURN_BUDGET {
            let Some((_, source, target, request)) = self
                .runtime
                .next_network_repair_expiry()
                .filter(|(expires, ..)| now >= *expires)
            else {
                break;
            };
            self.runtime
                .cancel_network_repair(source, target, None, request, now);
            let branch = FabricLocalBranch::for_repair(source, target);
            if let Some(lease) = self
                .registry
                .resolve_mut(target.destination)
                .and_then(|receiver| receiver.reliable_subscriber_branch(branch))
                .filter(|lease| lease.endpoint.branch == target.incarnation)
            {
                self.report_fabric_repair(
                    target.destination,
                    lease,
                    request,
                    pulsebeam_proto::data_control::RepairStatus::Expired,
                );
            }
        }
    }

    fn advance_fabric_repairs(&mut self, now: Instant, send: &impl Fn(ShardId, ShardFrame)) {
        use crate::{
            participant::data_fabric::OriginalResolution, route::fabric::local::RepairQueue,
        };
        use pulsebeam_proto::data_control::RepairStatus;

        for _ in 0..FABRIC_CONTROL_TURN_BUDGET {
            let Some((source, owner, work)) = self
                .runtime
                .next_fabric_data_repair()
                .filter(|(_, _, work)| work.due <= now)
            else {
                break;
            };
            let target = work.target;
            let request = work.request;
            let branch = FabricLocalBranch::for_repair(source, target);
            let lease = self
                .registry
                .reliable_receiver(target.destination, branch, self.fabric_unix_time())
                .filter(|lease| lease.endpoint.branch == target.incarnation)
                .and_then(|lease| Some((self.registry.connection_id(target.destination)?, lease)));
            let Some((connection, lease)) = lease else {
                if let Some(fanout) = self.runtime.repair_fanout(source) {
                    fanout.cancel_data_repair(target, request);
                }
                continue;
            };
            let Some(fanout) = self.runtime.repair_fanout(source) else {
                break;
            };
            let status = if now >= work.expires {
                Some(RepairStatus::Expired)
            } else if fanout.repair_until(now, target, request).is_none() {
                Some(RepairStatus::Revoked)
            } else {
                fanout.defer_data_repair(now, work);
                if work.queued {
                    self.drain_fabric_data_branch(branch, target.destination, now);
                    continue;
                }
                let resolution = match owner {
                    crate::shard::router::FabricRepairSource::Publisher(
                        publisher,
                        installation,
                    ) => {
                        if self.fabric_publisher_authorized(publisher, source) {
                            self.resolve_data_original(
                                publisher,
                                source,
                                installation,
                                request.fragment,
                                now,
                            )
                        } else {
                            OriginalResolution::Revoked
                        }
                    }
                    crate::shard::router::FabricRepairSource::QuicPublisher(publisher) => {
                        if self.fabric_publisher_authorized(publisher, source) {
                            self.resolve_quic_data_original(
                                publisher,
                                source,
                                request.fragment,
                                now,
                            )
                        } else {
                            OriginalResolution::Revoked
                        }
                    }
                    crate::shard::router::FabricRepairSource::Shard(binding) => {
                        if self.runtime.shard_fabric_source(binding) == Some(source)
                            && lease.endpoint.source == binding.incarnation
                        {
                            send(
                                binding.origin,
                                ShardFrame::FabricDataRepair(
                                    crate::route::fabric::ShardDataRepair {
                                        binding,
                                        receiver: crate::route::fabric::ShardDataReceiver {
                                            connection,
                                            branch: lease.endpoint.branch,
                                        },
                                        request,
                                    },
                                ),
                            );
                        }
                        continue;
                    }
                };
                match resolution {
                    OriginalResolution::Cached { sequence, expires } if now < expires => {
                        let Some(fanout) = self.runtime.repair_fanout(source) else {
                            continue;
                        };
                        match fanout.enqueue_data_repair(now, target, request, sequence) {
                            RepairQueue::Queued | RepairQueue::Backpressured => {
                                self.drain_fabric_data_branch(branch, target.destination, now);
                                None
                            }
                            RepairQueue::Unavailable => Some(RepairStatus::Unavailable),
                        }
                    }
                    OriginalResolution::Cached { .. } => Some(RepairStatus::Expired),
                    OriginalResolution::Unavailable => Some(RepairStatus::Unavailable),
                    OriginalResolution::Exhausted => Some(RepairStatus::Exhausted),
                    OriginalResolution::Revoked => Some(RepairStatus::Revoked),
                    OriginalResolution::Request(_) | OriginalResolution::Pending => None,
                }
            };
            if let Some(status) = status
                && let Some(fanout) = self.runtime.repair_fanout(source)
                && fanout.cancel_data_repair(target, request)
            {
                self.report_fabric_repair(target.destination, lease, request, status);
            }
        }
    }

    fn advance_publisher_data_expectations(&mut self, now: Instant) {
        use crate::route::fabric::PublisherExpectationStep;
        for _ in 0..FABRIC_CONTROL_TURN_BUDGET {
            let Some((_, source)) = self
                .runtime
                .next_publisher_data_expectation()
                .filter(|(deadline, _)| now >= *deadline)
            else {
                break;
            };
            match self.runtime.tick_publisher_data_expectation(source, now) {
                Some(PublisherExpectationStep::Send(expectation)) => {
                    if let Some((publisher, current)) = expectation
                        .route
                        .publisher
                        .data_publisher()
                        .and_then(|publisher| self.data_publisher_source(publisher))
                        && source == current
                        && self.fabric_publisher_authorized(publisher, current)
                    {
                        self.pipeline.push_publisher_data_expectation(expectation);
                    }
                }
                Some(PublisherExpectationStep::Settled(outcome)) => {
                    self.record_relay_data_outcome(outcome);
                }
                None => break,
            }
        }
    }

    fn advance_fabric_control(&mut self, now: Instant, send: impl Fn(ShardId, ShardFrame)) {
        self.advance_publisher_data_expectations(now);
        for _ in 0..FABRIC_CONTROL_TURN_BUDGET {
            let Some((deadline, source, publisher)) = self.runtime.next_shard_data_notice() else {
                break;
            };
            if now < deadline {
                break;
            }
            if self.fabric_publisher_authorized(publisher, source) {
                self.runtime.tick_shard_data_notice(source, now);
            }
        }
        self.retry_publisher_lookups(now);
        self.expire_network_repairs(now);
        self.advance_fabric_repairs(now, &send);
        use crate::{
            route::fabric::local::{ExpectationWrite, PropagationOutcome, PropagationStep},
            shard::router::{FabricDeliveryTarget, FabricOwner},
        };

        for _ in 0..FABRIC_CONTROL_TURN_BUDGET {
            let propagation = self.runtime.next_fabric_data_propagation();
            if let Some((deadline, source, target)) = self.runtime.next_fabric_delivery_report()
                && now >= deadline
                && propagation.is_none_or(|(next, _, _)| deadline <= next)
            {
                match target {
                    FabricDeliveryTarget::ShardShadow(binding) => {
                        let live = self.runtime.shard_fabric_source(binding) == Some(source);
                        self.runtime
                            .tick_fabric_delivery_report(source, now, |report| {
                                if !live
                                    || report.outcome
                                        != crate::route::fabric::local::DeliveryOutcome::Confirmed
                                {
                                    return ExpectationWrite::Revoked;
                                }
                                let receipt = crate::route::fabric::ShardDataReceipt {
                                    binding,
                                    unit: report.unit,
                                    branch: report.branch_incarnation,
                                };
                                #[cfg(feature = "sim")]
                                if crate::sim_metrics::drop_shard_data_receipt(receipt, now) {
                                    return ExpectationWrite::Backpressured;
                                }
                                send(binding.origin, ShardFrame::FabricDataReceipt(receipt));
                                // Mailbox acceptance does not confirm origin processing.
                                ExpectationWrite::Backpressured
                            });
                    }
                    FabricDeliveryTarget::QuicPublisher(publisher) => {
                        let endpoint = self
                            .runtime
                            .fabric_incarnation(source)
                            .filter(|_| self.fabric_publisher_authorized(publisher, source))
                            .map(|source| pulsebeam_proto::data_control::PublisherBinding {
                                source,
                            });
                        let registry = &mut self.registry;
                        let ready = &mut self.quic_data_ready;
                        self.runtime
                            .tick_fabric_delivery_report(source, now, |report| {
                                let Some(endpoint) = endpoint else {
                                    return ExpectationWrite::Revoked;
                                };
                                let record =
                                    crate::participant::data_fabric::publisher_delivery_record(
                                        endpoint, report,
                                    );
                                if registry
                                    .enqueue_quic_publisher_control(publisher, source, record)
                                {
                                    *ready = true;
                                    ExpectationWrite::Queued
                                } else {
                                    ExpectationWrite::Backpressured
                                }
                            });
                    }
                    FabricDeliveryTarget::Publisher(publisher, installation) => {
                        let authorized = self.fabric_publisher_authorized(publisher, source);
                        let (registry, dirty) = (&mut self.registry, &mut self.dirty);
                        self.runtime
                            .tick_fabric_delivery_report(source, now, |report| {
                                let Some(owner) =
                                    registry.resolve_mut(publisher).filter(|_| authorized)
                                else {
                                    return ExpectationWrite::Revoked;
                                };
                                let written = owner.write_fabric_delivery_report(
                                    source,
                                    installation,
                                    report,
                                );
                                if written == ExpectationWrite::Queued {
                                    dirty.mark(publisher, owner);
                                }
                                written
                            });
                    }
                }
                continue;
            }
            let Some((deadline, source, owner)) = propagation else {
                break;
            };
            if now < deadline {
                break;
            }
            let authorized = match owner {
                FabricOwner::Local(publisher) => {
                    self.fabric_publisher_authorized(publisher, source)
                }
                FabricOwner::Network => self.runtime.has_live_network_ingress(source),
                FabricOwner::Shard(binding) => {
                    self.runtime.shard_fabric_source(binding) == Some(source)
                }
            };
            let unix_now = self.fabric_unix_time();
            let (registry, dirty) = (&mut self.registry, &mut self.dirty);
            let quic_ready = &mut self.quic_data_ready;
            let step = self.runtime.tick_fabric_data_propagation(
                source,
                now,
                |branch, destination, unit, incarnation| {
                    if !authorized {
                        return ExpectationWrite::Revoked;
                    }
                    if registry.is_quic_owner(destination) {
                        let Some(lease) = registry
                            .reliable_receiver(destination, branch, unix_now)
                            .filter(|lease| lease.endpoint.branch == incarnation)
                        else {
                            return ExpectationWrite::Revoked;
                        };
                        use pulsebeam_proto::data_control::subscriber_observation::Record;
                        return if registry.enqueue_quic_subscriber_control(
                            destination,
                            lease,
                            Record::Expectation(lease.expectation(unit)),
                        ) {
                            *quic_ready = true;
                            ExpectationWrite::Queued
                        } else {
                            ExpectationWrite::Backpressured
                        };
                    }
                    let Some(receiver) = registry.resolve_mut(destination) else {
                        return ExpectationWrite::Revoked;
                    };
                    let written = receiver.write_fabric_expectation(branch, unit, incarnation);
                    if written == ExpectationWrite::Queued {
                        dirty.mark(destination, receiver);
                    }
                    written
                },
            );
            if let Some(PropagationStep::Settled { outcome, .. }) = step {
                let outcome = match outcome {
                    PropagationOutcome::Confirmed => "confirmed",
                    PropagationOutcome::Failed => "failed",
                    PropagationOutcome::Unknown => "unknown",
                };
                metrics::counter!("fabric_data_expectation_outcome", "outcome" => outcome)
                    .increment(1);
            }
        }
    }

    pub(crate) fn fire_timers(&mut self, now: Instant, send: impl Fn(ShardId, ShardFrame)) {
        let registry = &mut self.registry;
        let dirty = &mut self.dirty;
        self.timers.drain_expired(now, |key| {
            if let Some(participant) = registry.resolve_mut(key) {
                participant.input(crate::participant::ParticipantInput::Timeout(now));
                dirty.mark(key, participant);
            }
        });
        self.runtime.expire_endpoint_data(now);
        self.advance_fabric_control(now, send);
    }

    pub(crate) fn poll_and_flush_dirty(
        &mut self,
        udp_socket: &mut pulsebeam_udp::UdpSocket,
        budget: usize,
        router: &impl IngressTransport,
    ) -> usize {
        // Commands and update retirements can create transports after the
        // worker sampled its turn start. Advance the wheel and RTC from one
        // fresh wall-time sample at the poll boundary.
        let now = Instant::now();
        self.fire_timers(now, |destination, frame| {
            router.send_frame(destination, frame);
        });
        debug_assert!(budget > 0);
        debug_assert!(self.udp_send_batch.is_empty());
        self.dirty.begin_phase();
        let data_time = crate::participant::data_fabric::DataTime::from_reference(
            self.global_clock.as_mut().and_then(|clock| clock.now(now)),
        );
        let mut processed = 0;
        while processed < budget {
            let Some(key) = self.dirty.next() else {
                break;
            };
            processed = processed.saturating_add(1);
            let Some(participant) = self.registry.resolve_mut(key) else {
                continue;
            };
            participant.queued_dirty = false;
            let who = crate::shard::events::ParticipantBinding {
                participant_id: participant.participant_id,
                connection_id: participant.connection_id,
                handle: key,
            };
            let mut sink = self.pipeline.polling_sink(who, &mut self.runtime);
            sink.data_time = data_time;
            sink.media_clock = self.global_clock.as_ref();
            if let Some(deadline) = participant.poll(now, &mut sink) {
                self.timers.schedule(key, deadline);
            }
            self.drain_participant_network(key, udp_socket);
        }
        if !self.dirty.exhausted() {
            self.dirty.finish_partial();
        } else {
            self.dirty.finish_phase();
        }
        self.flush_udp(udp_socket);
        processed
    }
}

#[cfg(test)]
mod fabric_tests;
#[cfg(test)]
pub(crate) use fabric_tests::FabricRtpFixture;

#[cfg(test)]
mod wrong_owner_tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use crate::control::ufrag::IceUfrag;
    use crate::id::ShardId;
    use crate::route::TransportRoute;
    use pulsebeam_runtime::net::{RecvPacketBatch, Transport, UdpMode};

    const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
    const BINDING_REQUEST: [u8; 2] = [0x00, 0x01];
    const USERNAME_TYPE: [u8; 2] = [0x00, 0x06];

    fn stun_binding_request(server_ufrag: &str) -> Vec<u8> {
        let username = format!("{server_ufrag}:client");
        let value = username.as_bytes();
        let padded = value.len().next_multiple_of(4);
        let attr_total = 4 + padded;

        let mut buf = Vec::with_capacity(20 + attr_total);
        buf.extend_from_slice(&BINDING_REQUEST);
        buf.extend_from_slice(&u16::try_from(attr_total).unwrap().to_be_bytes());
        buf.extend_from_slice(&MAGIC_COOKIE);
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(&USERNAME_TYPE);
        buf.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(value);
        buf.resize(buf.len() + (padded - value.len()), 0);
        buf
    }

    /// The compatibility entry point has no transport for forwarding, so a
    /// foreign datagram is counted and discarded. Workers use
    /// `on_udp_batch_routed`, which forwards the same datagram to its owner.
    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn a_datagram_for_another_shard_is_dropped_not_asserted() {
        let shard = ShardId::new(0);
        let (_writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            4,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );

        let foreign = TransportRoute::new(ShardId::new(3), 41);
        let data = stun_binding_request(&IceUfrag::new(0, 0, foreign, 9).encode());
        let len = data.len();
        core.execution.on_udp_batch(RecvPacketBatch {
            src: "203.0.113.7:40000".parse().unwrap(),
            dst: "198.51.100.1:3478".parse().unwrap(),
            buf: data,
            stride: len,
            len,
            transport: Transport::Udp(UdpMode::Scalar),
            received_at: pulsebeam_runtime::net::ReceiveTime::now(),
            offset: 0,
        });

        assert_eq!(
            core.participant_count(),
            0,
            "a foreign-shard datagram must not create or touch local state"
        );
    }

    struct CaptureTransport {
        frames: std::cell::RefCell<Vec<(ShardId, ShardFrame)>>,
    }

    impl IngressTransport for CaptureTransport {
        fn send_frame(&self, dst: ShardId, frame: ShardFrame) {
            self.frames.borrow_mut().push((dst, frame));
        }
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn a_datagram_for_another_shard_is_forwarded_to_its_owner() {
        let shard = ShardId::new(0);
        let (_writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            4,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );
        let router = CaptureTransport {
            frames: std::cell::RefCell::new(Vec::new()),
        };
        let foreign = TransportRoute::new(ShardId::new(3), 41);
        let data = stun_binding_request(&IceUfrag::new(0, 0, foreign, 9).encode());
        let len = data.len();

        core.on_udp_batch_routed(
            RecvPacketBatch {
                src: "203.0.113.7:40000".parse().unwrap(),
                dst: "198.51.100.1:3478".parse().unwrap(),
                buf: data,
                stride: len,
                len,
                transport: Transport::Udp(UdpMode::Scalar),
                received_at: pulsebeam_runtime::net::ReceiveTime::now(),
                offset: 0,
            },
            &router,
        );

        let mut frames = router.frames.borrow_mut();
        assert_eq!(frames.len(), 1);
        let (dst, frame) = frames.pop().expect("wrong-owner ingress must be forwarded");
        assert_eq!(dst, ShardId::new(3));
        assert!(matches!(frame, ShardFrame::Ingress { source_shard, .. } if source_shard == shard));
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn a_view_delta_is_applied_as_one_consistent_state_transition() {
        let shard = ShardId::new(0);
        let (mut writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );
        let publication_id = crate::entity::ConnectionId::new()
            .fixture_publication(crate::entity::TrackKind::Audio, "consistent");
        let route = crate::route::NodeRouteAddress::new(crate::route::RouteId::new(shard, 7), 1);

        writer.stage(
            1,
            crate::shard_update::ShardUpdateOp::InsertTrackRuntime {
                publication_id,
                runtime: crate::shard_update::TrackRuntime::default(),
            },
        );
        writer.stage(
            1,
            crate::shard_update::ShardUpdateOp::InstallRoute {
                address: route,
                action: crate::shard_update::TrackRouteAction::Forward { publication_id },
            },
        );
        let plans = vec![crate::shard_update::TrackPlanUpdate {
            publication_id,
            plan: Some(crate::shard_update::TrackPlan::default()),
        }];
        writer.stage_plans(1, plans);
        assert_eq!(writer.publish(), Some(1));

        assert_eq!(core.apply_updates(1), 1);
        assert!(core.execution.runtime.routes.resolve(route).is_some());
        let plans = &core.execution.plans;
        let track_handle = core.execution.runtime.track_handle(publication_id).unwrap();
        assert!(plans.get(track_handle).is_some());
    }

    #[test]
    fn connected_flow_reports_its_registered_transport_and_owner() {
        use crate::participant::event::ParticipantSink;
        use crate::shard::events::ParticipantBinding;

        let host_ip = pulsebeam_runtime::testing::test_host_ip("192.168.250.13");
        pulsebeam_runtime::testing::run_local(host_ip, async move {
            #[cfg(not(feature = "sim"))]
            pulsebeam_runtime::time::pause();

            let owner = ShardId::new(2);
            let (_writer, update_rx) = crate::shard_update::new_shard_update(owner);
            let mut core = ShardCore::new(
                owner,
                4,
                1,
                WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
                update_rx,
            );
            let router = CaptureTransport {
                frames: std::cell::RefCell::new(Vec::new()),
            };
            let participant_id = crate::entity::fixture_participant_id();
            let connection_id = crate::entity::ConnectionId::new();
            let room_id = crate::entity::RoomId::new("connected-flow").unwrap();
            let address = NodeTransportAddress::new(TransportRoute::new(owner, 51), 7);
            let handoffs = std::cell::RefCell::new(Vec::new());
            let materialize = |core: &mut ShardCore| {
                let mut resources = crate::control::NegotiatedResources::empty_for_test();
                handoffs
                    .borrow_mut()
                    .push(crate::control::native::fixture::execution_handoff(
                        &mut resources,
                        connection_id,
                    ));
                let (ack, received) = pulsebeam_runtime::sync::oneshot::channel();
                core.on_command(
                    ShardCommand::MaterializeParticipant {
                        transport: address,
                        config: Box::new(ConnectionConfig {
                            scope: crate::entity::ProjectRoomKey::fixture(room_id).into(),
                            participant_id,
                            connection_id,
                            profile: crate::control::controller::SignalingProtocol::State,
                            authorization_expiry: None,
                            rtc: crate::control::application_only_test_connection(),
                            resources,
                        }),
                        ack,
                    },
                    &router,
                );
                received
            };
            #[cfg(feature = "sim")]
            {
                crate::sim_metrics::fail_next_materialization();
                assert!(!materialize(&mut core).await.unwrap());
                assert_eq!(core.participant_count(), 0);
                assert!(
                    core.execution
                        .registry
                        .resolve_endpoint(EndpointKey {
                            room: crate::entity::ProjectRoomKey::fixture(room_id),
                            participant: participant_id
                        })
                        .is_none()
                );
                assert!(core.execution.registry.resolve_transport(address).is_none());
            }
            assert!(materialize(&mut core).await.unwrap());
            let handle = core
                .execution
                .registry
                .resolve_endpoint(EndpointKey {
                    room: crate::entity::ProjectRoomKey::fixture(room_id),
                    participant: participant_id,
                })
                .unwrap();
            assert_eq!(
                core.execution.registry.resolve_transport(address),
                Some(handle)
            );
            let source = "203.0.113.7:40000".parse().unwrap();
            let destination = "198.51.100.1:3478".parse().unwrap();
            let ingress = ShardId::new(0);
            core.execution.registry.learn_addr(source, address);
            core.execution
                .pipeline
                .participant_sink(ParticipantBinding {
                    participant_id,
                    connection_id,
                    handle,
                })
                .connected(source, destination, ingress);
            assert_eq!(core.flush_participant_events(&router, 1), 1);
            assert!(matches!(
                core.pop_shard_event(),
                Some(ShardEvent::TransportAuthenticated {
                    source: actual_source,
                    destination: actual_destination,
                    source_shard,
                    address: actual_address,
                    shard,
                }) if actual_source == source && actual_destination == destination
                    && source_shard == ingress && actual_address == address && shard == owner
            ));
            assert!(core.pop_shard_event().is_none());
        });
    }

    #[test]
    fn replacement_waits_for_old_plans_to_release_the_participant_handle() {
        use crate::route::NodeTransportAddress;
        use crate::shard_update::{ShardUpdateOp, TrackPlan, TrackPlanUpdate, TrackRuntime};
        use std::io::{Read, Seek};
        use std::time::Duration;
        use tracing_subscriber::fmt::format::FmtSpan;

        const ROOM: &str = "RoomTraceSentinel";
        const PARTICIPANT: &str = "ParticipantTraceSentinel";

        let host_ip = pulsebeam_runtime::testing::test_host_ip("192.168.250.12");
        pulsebeam_runtime::testing::run_local(host_ip, async move {
            #[cfg(not(feature = "sim"))]
            pulsebeam_runtime::time::pause();

            let shard = ShardId::new(0);
            let (mut writer, update_rx) = crate::shard_update::new_shard_update(shard);
            let mut core = ShardCore::new(
                shard,
                4,
                1,
                WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
                update_rx,
            );
            let router = CaptureTransport {
                frames: std::cell::RefCell::new(Vec::new()),
            };
            let participant = crate::entity::ParticipantId::new(PARTICIPANT).unwrap();
            let old_address = NodeTransportAddress::new(TransportRoute::new(shard, 51), 1);
            let new_address = NodeTransportAddress::new(TransportRoute::new(shard, 52), 1);
            let handoffs = std::cell::RefCell::new(Vec::new());
            let config = || {
                let connection_id = crate::entity::ConnectionId::new();
                let mut resources = crate::control::NegotiatedResources::empty_for_test();
                handoffs
                    .borrow_mut()
                    .push(crate::control::native::fixture::execution_handoff(
                        &mut resources,
                        connection_id,
                    ));
                ConnectionConfig {
                    scope: crate::entity::ProjectRoomKey::fixture(
                        crate::entity::RoomId::new(ROOM).unwrap(),
                    )
                    .into(),
                    participant_id: participant,
                    connection_id,
                    profile: crate::control::controller::SignalingProtocol::State,
                    authorization_expiry: None,
                    rtc: crate::control::application_only_test_connection(),
                    resources,
                }
            };
            let materialize = |core: &mut ShardCore, address, config| {
                let (ack, received) = pulsebeam_runtime::sync::oneshot::channel();
                core.on_command(
                    ShardCommand::MaterializeParticipant {
                        transport: address,
                        config: Box::new(config),
                        ack,
                    },
                    &router,
                );
                received
            };
            let old = config();
            let old_id = old.connection_id;
            let endpoint = EndpointKey {
                room: old.scope.joined_room().unwrap(),
                participant,
            };
            let mut trace = tempfile::tempfile().unwrap();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_span_events(FmtSpan::NEW)
                .with_writer(trace.try_clone().unwrap())
                .finish();
            let received = tracing::subscriber::with_default(subscriber, || {
                materialize(&mut core, old_address, old)
            });
            assert!(received.await.unwrap());
            trace.rewind().unwrap();
            let mut output = String::new();
            trace.read_to_string(&mut output).unwrap();
            assert!(output.contains("participant added to shard"));
            assert!(output.contains(&format!("connection_id={old_id}")));
            assert!(!output.contains(ROOM));
            assert!(!output.contains(PARTICIPANT));
            let old_handle = core.execution.registry.resolve_endpoint(endpoint).unwrap();
            assert_eq!(
                core.execution.registry.resolve_connection(old_id),
                Some(old_handle)
            );
            let publication_id = crate::entity::ConnectionId::new()
                .fixture_publication(crate::entity::TrackKind::Audio, "mic");
            writer.stage(
                1,
                ShardUpdateOp::InsertTrackRuntime {
                    publication_id,
                    runtime: TrackRuntime::default(),
                },
            );
            writer.stage_plans(
                1,
                vec![TrackPlanUpdate {
                    publication_id,
                    plan: Some(TrackPlan::new([old_id], [], None)),
                }],
            );
            writer.publish();
            assert_eq!(core.apply_updates(1), 1);
            let track_handle = core.execution.runtime.track_handle(publication_id).unwrap();
            assert_eq!(
                core.execution.plans.get(track_handle).unwrap().local,
                vec![old_handle]
            );

            // A rejected candidate and its successor arrive before plan cleanup.
            let rejected_address = NodeTransportAddress::new(TransportRoute::new(shard, 53), 1);
            let rejected = config();
            let rejected_id = rejected.connection_id;
            assert!(
                materialize(&mut core, rejected_address, rejected)
                    .await
                    .unwrap()
            );
            let replacement = config();
            let replacement_id = replacement.connection_id;
            assert!(
                materialize(&mut core, new_address, replacement)
                    .await
                    .unwrap()
            );
            assert_eq!(
                core.execution.registry.resolve_connection(replacement_id),
                None
            );
            assert_eq!(
                core.execution.registry.resolve_endpoint(endpoint),
                Some(old_handle)
            );
            assert_eq!(core.execution.registry.resolve_transport(new_address), None);
            assert_eq!(
                core.execution.plans.get(track_handle).unwrap().local,
                vec![old_handle]
            );

            writer.stage_plans(
                2,
                vec![TrackPlanUpdate {
                    publication_id,
                    plan: Some(TrackPlan::default()),
                }],
            );
            writer.stage(
                2,
                ShardUpdateOp::RemoveParticipant {
                    connection: rejected_id,
                    address: rejected_address,
                },
            );
            writer.stage(
                2,
                ShardUpdateOp::RetireTransport {
                    address: old_address,
                },
            );
            writer.stage(
                2,
                ShardUpdateOp::RemoveParticipant {
                    connection: old_id,
                    address: old_address,
                },
            );
            writer.publish();
            let turn_started = Instant::now();
            pulsebeam_runtime::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(core.apply_updates(1), 1);
            let new_handle = core.execution.registry.resolve_endpoint(endpoint).unwrap();
            assert_ne!(new_handle, old_handle);
            assert_eq!(core.execution.registry.resolve_connection(old_id), None);
            assert_eq!(
                core.execution.registry.resolve_connection(replacement_id),
                Some(new_handle)
            );
            assert!(core.execution.pending_replacements.is_empty());
            assert_eq!(
                core.execution.registry.resolve_transport(rejected_address),
                None
            );
            assert_eq!(
                core.execution.registry.resolve_transport(new_address),
                Some(new_handle)
            );
            assert!(core.execution.registry.resolve_mut(old_handle).is_none());
            assert!(
                core.execution
                    .plans
                    .get(track_handle)
                    .unwrap()
                    .local
                    .is_empty()
            );

            let mut udp = pulsebeam_udp::bind(pulsebeam_udp::BindOptions::new(
                "127.0.0.1:0".parse().unwrap(),
            ))
            .await
            .unwrap()
            .activate()
            .unwrap();
            let wall_now = Instant::now();
            assert!(wall_now > turn_started + pulsebeam_runtime::SHARD_TIMER_QUANTUM);
            assert_eq!(core.poll_and_flush_dirty(&mut udp, 256, &router), 2);
            let deadline = core.next_timer_deadline().unwrap();
            assert!(deadline >= wall_now);
            assert!(deadline <= wall_now + Duration::from_millis(101));

            writer.stage(
                3,
                ShardUpdateOp::RemoveParticipant {
                    connection: old_id,
                    address: old_address,
                },
            );
            writer.publish();
            assert_eq!(core.apply_updates(1), 1);
            assert_eq!(
                core.execution.registry.resolve_endpoint(endpoint),
                Some(new_handle)
            );
            assert!(core.next_timer_deadline().is_some());

            let mut other = config();
            other.scope.project = crate::entity::ProjectId::new();
            let other_connection = other.connection_id;
            let other_endpoint = other.scope.endpoint(participant).unwrap();
            let other_address = NodeTransportAddress::new(TransportRoute::new(shard, 54), 1);
            assert!(materialize(&mut core, other_address, other).await.unwrap());
            let other_handle = core
                .execution
                .registry
                .resolve_endpoint(other_endpoint)
                .unwrap();
            assert_ne!(other_handle, new_handle);
            assert_eq!(
                core.execution.registry.resolve_connection(other_connection),
                Some(other_handle)
            );
            assert_eq!(
                core.execution.registry.resolve_endpoint(endpoint),
                Some(new_handle)
            );
            assert!(core.execution.pending_replacements.is_empty());

            writer.stage_plans(
                4,
                vec![TrackPlanUpdate {
                    publication_id,
                    plan: Some(TrackPlan::new([replacement_id], [], None)),
                }],
            );
            writer.publish();
            assert_eq!(core.apply_updates(1), 1);
            assert_eq!(
                core.execution.plans.get(track_handle).unwrap().local,
                [new_handle]
            );
        });
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn compiled_network_reverse_overrides_the_local_reverse_route() {
        let shard = ShardId::new(0);
        let (_, update_rx) = crate::shard_update::new_shard_update(shard);
        let origin = Instant::now();
        let mut core = ShardCore::new(
            shard,
            4,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, origin),
            update_rx,
        );
        let publication_id = crate::entity::ConnectionId::from_bytes([6; 16])
            .fixture_publication(crate::entity::TrackKind::Video, "compiled");
        let handle = core
            .execution
            .runtime
            .install_track(publication_id, None, None, None);
        let mut control =
            pulsebeam_link::LinkControl::new(|| pulsebeam_link::LinkTime::from_nanos(0)).unwrap();
        let (port, _rx) = control.create_port().unwrap();
        // Plan installation treats compiled handles opaquely; transport behavior
        // is covered by Link's two-association tests, not duplicated here.
        let mut compiled = |slot| {
            let address =
                crate::route::NodeRouteAddress::new(crate::route::RouteId::new(shard, slot), 1);
            let (_, tx) = control
                .bind_send(pulsebeam_link::Destination::Local {
                    port,
                    tag: crate::link::delivery_tag(address),
                })
                .unwrap();
            crate::shard_update::CompiledRoute {
                address,
                tx,
                clock_origin: origin,
            }
        };
        let local = compiled(1);
        let reverse = compiled(2);
        let plan = crate::shard_update::TrackPlan {
            remote: vec![local.address],
            reverse_route: Some(local.address),
            compiled_remote: vec![local.clone()],
            compiled_reverse: Some(local.clone()),
            compiled_network_reverse: Some(reverse.clone()),
            ..Default::default()
        };
        assert_eq!(
            core.execution
                .apply_plan(&crate::shard_update::TrackPlanUpdate {
                    publication_id,
                    plan: Some(plan)
                }),
            2
        );
        let installed = core.execution.plans.get(handle).unwrap();
        assert_eq!(installed.remote, vec![local]);
        assert_eq!(installed.reverse_route, Some(reverse));
    }

    #[cfg(debug_assertions)]
    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    #[should_panic(expected = "track plan removal must precede runtime retirement")]
    async fn runtime_retirement_rejects_a_live_plan() {
        let shard = ShardId::new(0);
        let (_, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );
        let publication_id = crate::entity::ConnectionId::new()
            .fixture_publication(crate::entity::TrackKind::Audio, "audio");
        core.execution
            .runtime
            .install_track(publication_id, None, None, None);
        core.execution
            .apply_plan(&crate::shard_update::TrackPlanUpdate {
                publication_id,
                plan: Some(crate::shard_update::TrackPlan::default()),
            });
        core.execution.apply_lifecycle_op(
            &crate::shard_update::ShardUpdateOp::RemoveTrackRuntime { publication_id },
        );
    }

    #[pulsebeam_runtime::test(start_paused = true, crate = "pulsebeam_runtime::__harness")]
    async fn route_retirement_waits_for_the_bounded_plan_cursor() {
        let shard = ShardId::new(0);
        let (mut writer, update_rx) = crate::shard_update::new_shard_update(shard);
        let mut core = ShardCore::new(
            shard,
            4,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, Instant::now()),
            update_rx,
        );
        let route = crate::route::NodeRouteAddress::new(crate::route::RouteId::new(shard, 8), 1);
        let connection = crate::entity::ConnectionId::new();
        let track_ids = (0..=crate::shard::worker::SHARD_PLAN_OPERATION_BUDGET)
            .map(|index| {
                connection.fixture_publication(crate::entity::TrackKind::Audio, &index.to_string())
            })
            .collect::<Vec<_>>();
        for publication_id in &track_ids {
            writer.stage(
                1,
                crate::shard_update::ShardUpdateOp::InsertTrackRuntime {
                    publication_id: *publication_id,
                    runtime: crate::shard_update::TrackRuntime::default(),
                },
            );
        }
        let plans = track_ids
            .iter()
            .map(|publication_id| crate::shard_update::TrackPlanUpdate {
                publication_id: *publication_id,
                plan: Some(crate::shard_update::TrackPlan::default()),
            })
            .collect();

        writer.stage(
            1,
            crate::shard_update::ShardUpdateOp::InstallRoute {
                address: route,
                action: crate::shard_update::TrackRouteAction::Forward {
                    publication_id: track_ids[0],
                },
            },
        );
        writer.stage_plans(1, plans);
        writer.stage(
            1,
            crate::shard_update::ShardUpdateOp::RetireRoute { address: route },
        );
        assert_eq!(writer.publish(), Some(1));
        let mut cutoff = writer.stage_cutoff(2);
        assert_eq!(writer.publish(), Some(2));

        assert_eq!(core.apply_updates(1), 0);
        assert!(core.execution.runtime.routes.resolve(route).is_some());
        assert!(matches!(
            cutoff.try_recv(),
            Err(pulsebeam_runtime::sync::oneshot::error::TryRecvError::Empty)
        ));

        assert_eq!(core.apply_updates(1), 1);
        assert!(core.execution.runtime.routes.resolve(route).is_none());
        assert!(matches!(
            cutoff.try_recv(),
            Err(pulsebeam_runtime::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(core.apply_updates(1), 1);
        assert_eq!(cutoff.await, Ok(()));

        writer.stage(
            3,
            crate::shard_update::ShardUpdateOp::InstallRoute {
                address: route,
                action: crate::shard_update::TrackRouteAction::Forward {
                    publication_id: track_ids[0],
                },
            },
        );
        writer.stage_plans(
            3,
            track_ids
                .iter()
                .map(|publication_id| crate::shard_update::TrackPlanUpdate {
                    publication_id: *publication_id,
                    plan: None,
                })
                .collect(),
        );
        writer.stage(
            3,
            crate::shard_update::ShardUpdateOp::RetireRoute { address: route },
        );
        for publication_id in &track_ids {
            writer.stage(
                3,
                crate::shard_update::ShardUpdateOp::RemoveTrackRuntime {
                    publication_id: *publication_id,
                },
            );
        }
        assert_eq!(writer.publish(), Some(3));
        assert_eq!(core.apply_updates(1), 0);
        assert_eq!(core.execution.plans.len(), 1);
        assert!(core.execution.runtime.routes.resolve(route).is_some());
        assert!(
            track_ids
                .iter()
                .all(|id| core.execution.runtime.track_handle(*id).is_some())
        );

        assert_eq!(core.apply_updates(1), 1);
        assert!(core.execution.plans.is_empty());
        assert!(core.execution.runtime.routes.resolve(route).is_none());
        assert!(
            track_ids
                .iter()
                .all(|id| core.execution.runtime.track_handle(*id).is_none())
        );
    }
}
