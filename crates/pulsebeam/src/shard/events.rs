use std::collections::VecDeque;

use super::worker::ShardEvent;
use crate::entity::{ConnectionId, ParticipantId};
use crate::keys::ParticipantHandle;
use crate::keys::TrackHandle;
#[cfg(test)]
use crate::participant::event::FabricPublication;
use crate::participant::event::{FabricDataPosition, FabricSubscriberFeedback, ParticipantSink};
use crate::participant::reverse::ReversePacket;
use crate::track::{Track, TrackMeta};

/// The shard-local binding of the participant emitting into the pipeline.
///
/// A sink is built per participant in the dirty loop, which already holds
/// both the handle and stable identity, so carrying both costs nothing and
/// lets hot events use the handle while lifecycle events keep using the ID.
#[derive(Clone, Copy)]
pub(crate) struct ParticipantBinding {
    pub participant_id: ParticipantId,
    pub connection_id: ConnectionId,
    pub handle: ParticipantHandle,
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
        connection_id: ConnectionId,
        track: TrackMeta,
    },
    Deactivated {
        subscriber: ParticipantId,
        connection_id: ConnectionId,
        track: TrackMeta,
    },
}

pub enum ParticipantLifecycleEvent {
    Connected {
        participant_key: ParticipantHandle,
        source: std::net::SocketAddr,
        destination: std::net::SocketAddr,
        source_shard: crate::id::ShardId,
    },
    Exited {
        participant_id: ParticipantId,
        connection_id: ConnectionId,
    },
}

pub enum ShardInternalEvent {
    FabricSubscriberFeedback {
        destination: ParticipantHandle,
        feedback: FabricSubscriberFeedback,
    },
    StageFabricReceiver {
        receiver: ParticipantHandle,
        work: crate::control::native::Preparation,
    },
    StageLocalRtpReceiver {
        receiver: ParticipantHandle,
        reference: u64,
    },
    StageFabricPublisher {
        publisher: ParticipantHandle,
        work: crate::control::native::Preparation,
        publication: crate::entity::PublicationKey,
    },
    FenceFabricSource {
        publisher: ParticipantHandle,
        source: crate::shard::router::FabricSourceHandle,
    },
    FabricRefresh {
        destination: ParticipantHandle,
        branch: crate::shard::router::FabricLocalBranch,
    },
    ReverseRequested {
        stream: TrackHandle,
        packet: ReversePacket,
    },
}

pub(super) enum QueuedPublication {
    #[cfg(test)]
    Fabric {
        publisher: ParticipantHandle,
        publication: FabricPublication,
    },
    FabricData {
        publisher: ParticipantHandle,
        publication: FabricDataPosition,
    },
    FabricMedia {
        publisher: ParticipantHandle,
        source: crate::shard::router::FabricSourceHandle,
        input: crate::rtp::fabric_ingress::MediaIngress,
    },
}

pub(crate) struct EventPipeline {
    participant_events: VecDeque<ParticipantEvent>,
    track_queue: VecDeque<QueuedPublication>,
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

    pub fn participant_sink(&mut self, who: ParticipantBinding) -> PipelineSinkRef<'_> {
        PipelineSinkRef {
            id: who.participant_id,
            connection_id: who.connection_id,
            key: who.handle,
            pipeline: self,
            runtime: None,
        }
    }

    pub(super) fn polling_sink<'a>(
        &'a mut self,
        who: ParticipantBinding,
        runtime: &'a mut super::router::ShardRuntime,
    ) -> PipelineSinkRef<'a> {
        let mut sink = self.participant_sink(who);
        sink.runtime = Some(runtime);
        sink
    }

    pub fn pop_participant_event(&mut self) -> Option<ParticipantEvent> {
        self.participant_events.pop_front()
    }

    fn push_packet(&mut self, packet: QueuedPublication) -> bool {
        if self.track_queue.len() >= self.packet_capacity {
            metrics::counter!("routing_drop", "lane" => "packet", "stage" => "pipeline", "origin" => "local").increment(1);
            #[cfg(feature = "sim")]
            crate::sim_metrics::record_routing_drop("packet", "pipeline", "local");
            return false;
        }
        self.track_queue.push_back(packet);
        true
    }

    pub(super) fn push_publisher_data_expectation(
        &mut self,
        expectation: crate::route::fabric::PublisherExpectation,
    ) {
        if self.shard_events.len() < self.packet_capacity {
            self.push_shard_event(ShardEvent::PublisherDataExpectation(expectation));
        }
    }

    pub(super) fn push_network_data_repair(
        &mut self,
        event: crate::route::fabric::network_repair::InterestEvent,
    ) {
        if self.shard_events.len() < self.packet_capacity {
            self.push_shard_event(ShardEvent::NetworkDataRepair(Box::new(event)));
        }
    }

    pub(super) fn push_network_media_repair(
        &mut self,
        interest: crate::route::fabric::network_repair::MediaInterest,
    ) {
        if self.shard_events.len() < self.packet_capacity {
            self.push_shard_event(ShardEvent::NetworkMediaRepair(Box::new(interest)));
        }
    }

    pub(super) fn request_fabric_refresh(
        &mut self,
        destination: ParticipantHandle,
        branch: crate::shard::router::FabricLocalBranch,
    ) {
        if self.participant_events.len() < self.packet_capacity {
            self.participant_events
                .push_back(ParticipantEvent::Internal(
                    ShardInternalEvent::FabricRefresh {
                        destination,
                        branch,
                    },
                ));
        }
    }

    pub fn push_shard_event(&mut self, ev: ShardEvent) {
        self.shard_events.push_back(ev);
    }

    pub(super) fn pop_publication(&mut self) -> Option<QueuedPublication> {
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
    connection_id: ConnectionId,
    key: ParticipantHandle,
    pipeline: &'a mut EventPipeline,
    runtime: Option<&'a mut super::router::ShardRuntime>,
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
                    connection_id: self.connection_id,
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
                    connection_id: self.connection_id,
                    track,
                },
            ));
    }

    #[inline]
    fn publish_track(&mut self, track: Track) {
        debug_assert_eq!(track.meta().origin, self.id);
        debug_assert_eq!(track.meta().connection_id, self.connection_id);
        let mut track = track;
        track.set_reverse(None);
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ShardEvent::TrackPublished(track)));
    }

    #[inline]
    fn unpublish_track(&mut self, meta: TrackMeta) {
        debug_assert_eq!(meta.origin, self.id);
        debug_assert_eq!(meta.connection_id, self.connection_id);
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ShardEvent::TrackUnpublished(
                meta,
            )));
    }

    #[inline]
    fn unpublish_owned_track(&mut self, meta: TrackMeta, reference: u64) {
        debug_assert_eq!(meta.origin, self.id);
        debug_assert_eq!(meta.connection_id, self.connection_id);
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::OwnedTrackUnpublished { meta, reference },
            ));
    }

    fn set_owned_subscription(
        &mut self,
        reference: u64,
        source_room: crate::entity::ProjectRoomKey,
        track: crate::entity::PublicationKey,
        enabled: bool,
    ) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::OwnedSubscriptionSet {
                    connection_id: self.connection_id,
                    request: crate::control::topology::OwnedSubscription {
                        source_room,
                        subscriber: self.id,
                        reference,
                        track,
                        enabled,
                    },
                },
            ));
    }

    fn unsubscribe_owned(&mut self, reference: u64, source_room: crate::entity::ProjectRoomKey) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(
                ShardEvent::OwnedSubscriptionRemoved {
                    source_room,
                    subscriber: self.id,
                    connection_id: self.connection_id,
                    reference,
                },
            ));
    }

    #[inline]
    fn request_fabric_refresh(&mut self, branch: crate::shard::router::FabricLocalBranch) {
        self.pipeline.request_fabric_refresh(self.key, branch);
    }

    fn request_reverse(&mut self, stream: TrackHandle, packet: ReversePacket) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::ReverseRequested { stream, packet },
            ));
    }

    #[inline]
    fn renew_authorization(&mut self, token: String) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Control(ShardEvent::RenewAuthorization {
                participant: self.id,
                connection_id: self.connection_id,
                token: crate::shard::worker::RenewalToken(token),
            }));
    }

    #[inline]
    fn exit(&mut self) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Lifecycle(
                ParticipantLifecycleEvent::Exited {
                    participant_id: self.id,
                    connection_id: self.connection_id,
                },
            ));
    }

    #[inline]
    #[cfg(test)]
    fn publish_fabric_packet(&mut self, publication: FabricPublication) {
        self.pipeline.push_packet(QueuedPublication::Fabric {
            publisher: self.key,
            publication,
        });
    }

    fn publish_fabric_data(
        &mut self,
        source: super::router::FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
        bytes: &[u8],
        now: pulsebeam_runtime::time::Instant,
        stamp: u32,
    ) -> Result<(), crate::participant::data_fabric::Error> {
        let (publisher, fanout) = self
            .runtime
            .as_deref_mut()
            .and_then(|runtime| runtime.data_publisher_mut(source, self.key, installation))
            .ok_or(crate::participant::data_fabric::Error::Fenced)?;
        let publication = publisher.publish(fanout, bytes, now, stamp)?;
        self.pipeline.push_packet(QueuedPublication::FabricData {
            publisher: self.key,
            publication,
        });
        Ok(())
    }

    fn settle_fabric_original_unavailable(
        &mut self,
        source: super::router::FabricSourceHandle,
        installation: pulsebeam_rtc::DataChannelInstallation,
        demand: pulsebeam_proto::data_control::OriginalDemand,
        now: pulsebeam_runtime::time::Instant,
    ) {
        if let Some((publisher, fanout)) = self
            .runtime
            .as_deref_mut()
            .and_then(|runtime| runtime.data_publisher_mut(source, self.key, installation))
        {
            publisher.original_unavailable(fanout, demand, now);
        }
    }

    fn observe_fabric_sender_report(
        &mut self,
        source: crate::shard::router::FabricSourceHandle,
        now: pulsebeam_runtime::time::Instant,
        report: pulsebeam_rtc::SenderReport,
    ) {
        if let Some(runtime) = &mut self.runtime {
            runtime.observe_fabric_sender_report(source, self.key, now, report);
        }
    }

    fn publish_fabric_media(
        &mut self,
        source: crate::shard::router::FabricSourceHandle,
        input: crate::rtp::fabric_ingress::MediaIngress,
    ) {
        self.pipeline.push_packet(QueuedPublication::FabricMedia {
            publisher: self.key,
            source,
            input,
        });
    }

    fn observe_fabric_subscriber(&mut self, feedback: FabricSubscriberFeedback) {
        if self.pipeline.participant_events.len() >= self.pipeline.packet_capacity {
            return;
        }
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::FabricSubscriberFeedback {
                    destination: self.key,
                    feedback,
                },
            ));
    }

    fn stage_fabric_receiver(&mut self, work: crate::control::native::Preparation) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::StageFabricReceiver {
                    receiver: self.key,
                    work,
                },
            ));
    }

    fn stage_local_rtp_receiver(&mut self, reference: u64) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::StageLocalRtpReceiver {
                    receiver: self.key,
                    reference,
                },
            ));
    }

    fn stage_fabric_publisher(
        &mut self,
        work: crate::control::native::Preparation,
        publication: crate::entity::PublicationKey,
    ) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::StageFabricPublisher {
                    publisher: self.key,
                    work,
                    publication,
                },
            ));
    }

    fn fence_fabric_source(&mut self, source: crate::shard::router::FabricSourceHandle) {
        self.pipeline
            .participant_events
            .push_back(ParticipantEvent::Internal(
                ShardInternalEvent::FenceFabricSource {
                    publisher: self.key,
                    source,
                },
            ));
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;
    use pulsebeam_core::fabric::{FabricPacket, Header, Profile};

    fn packet(sequence: u32) -> FabricPublication {
        FabricPublication {
            source: Default::default(),
            arrival: pulsebeam_runtime::time::Instant::now(),
            sequence: u64::from(sequence),
            reliable: None,
            packet: FabricPacket::new(
                Header {
                    profile: Profile::Opus,
                    marker: false,
                    sequence,
                    timestamp: 480,
                    ingress_ntp32: 0,
                },
                &[],
                Box::<[u8]>::from([0xf8, 0xff, 0xfe]),
            )
            .unwrap(),
        }
    }

    fn identity() -> ParticipantBinding {
        ParticipantBinding {
            participant_id: crate::entity::fixture_participant_id(),
            connection_id: ConnectionId::new(),
            handle: ParticipantHandle::default(),
        }
    }

    #[test]
    fn every_event_lands_in_its_own_queue() {
        let mut pipeline = EventPipeline::with_capacity(4);
        let who = identity();

        let mut sink = pipeline.participant_sink(who);
        sink.publish_fabric_packet(packet(1));
        sink.exit();

        assert!(
            pipeline.pop_publication().is_some(),
            "RTP went to the track queue"
        );
        assert!(
            pipeline.pop_participant_event().is_some(),
            "lifecycle went to participant events"
        );

        assert!(pipeline.pop_publication().is_none());
        assert!(pipeline.pop_participant_event().is_none());
    }

    #[test]
    fn exit_carries_the_exact_connection_incarnation() {
        let mut pipeline = EventPipeline::with_capacity(1);
        let who = identity();

        pipeline.participant_sink(who).exit();

        assert!(matches!(
            pipeline.pop_participant_event(),
            Some(ParticipantEvent::Lifecycle(
                ParticipantLifecycleEvent::Exited {
                    participant_id,
                    connection_id,
                }
            )) if participant_id == who.participant_id && connection_id == who.connection_id
        ));
    }

    #[test]
    fn the_rtp_packet_queue_is_bounded_and_preserves_order() {
        let mut pipeline = EventPipeline::with_capacity(2);
        let mut sink = pipeline.participant_sink(identity());
        for sequence in 1..=3 {
            sink.publish_fabric_packet(packet(sequence));
        }
        let drained: Vec<_> = std::iter::from_fn(|| pipeline.pop_publication())
            .map(|queued| match queued {
                QueuedPublication::Fabric { publication, .. } => publication.sequence,
                _ => panic!("expected a typed Fabric packet"),
            })
            .collect();
        assert_eq!(drained, vec![1, 2]);
    }

    /// `has_pending` decides whether the shard loops again, so it has to agree with every queue.
    /// One it does not check is work the shard parks on until something else happens to wake it.
    #[test]
    fn has_pending_agrees_with_every_queue() {
        let who = identity();

        let mut pipeline = EventPipeline::with_capacity(2);
        assert!(!pipeline.has_pending(), "an empty pipeline has no work");

        pipeline.push_shard_event(ShardEvent::ParticipantClosed {
            participant: who.participant_id,
            connection_id: who.connection_id,
        });
        assert!(pipeline.has_pending(), "a shard event is work");
        assert!(pipeline.pop_shard_event().is_some());
        assert!(!pipeline.has_pending());

        pipeline
            .participant_sink(who)
            .publish_fabric_packet(packet(1));
        assert!(pipeline.has_pending(), "so is a Fabric packet");
        assert!(pipeline.pop_publication().is_some());
        assert!(!pipeline.has_pending(), "and draining it clears the flag");
    }
}
