use pulsebeam_core::fabric::{ExtensionKind, FabricPacket, Header, Profile};

use super::*;
use crate::{
    control::{NegotiatedResources, controller::SignalingProtocol},
    entity::{ConnectionId, ParticipantId, ProjectRoomKey, RoomId, TrackKind},
    id::ShardId,
    route::TransportRoute,
    shard_update::{ShardUpdateOp, TrackPlan, TrackPlanUpdate},
};

#[test]
fn wan_child_receipt_requires_the_installed_plan_and_exact_window() {
    use crate::route::fabric::{LiveNetworkChild, NetworkPacket};
    use crate::route::{NodeRouteAddress, RouteId};
    use crate::shard::updates::ShardUpdateApplication;
    use crate::shard_update::{TrackRouteAction, new_shard_update};
    use pulsebeam_link::{
        DeliveryTag, FabricIngress, LinkControl, LinkTime, NetworkClass, NetworkPeer, NodeRouteKey,
        PeerRole, SecretCredential,
    };

    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let shard = ShardId::new(0);
    let now = Instant::now();
    let mut execution = ShardExecution::new(
        shard,
        8,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now),
    );
    let (receiver, _, _) = participant(&mut execution, 1, "receiver");
    let publication = receiver.fixture_publication(TrackKind::Audio, "fabric-network");
    execution
        .runtime
        .install_track(publication, None, None, None);
    let destination = NodeRouteAddress::new(RouteId::new(shard, 2), 1);
    let mut link = LinkControl::<ShardFrame>::new(|| LinkTime::from_nanos(0)).unwrap();
    let credential = SecretCredential::generate().unwrap();
    let association = link
        .open_network(
            NetworkPeer {
                endpoint: "127.0.0.1:17001".parse().unwrap(),
                expected_peer: credential.fingerprint(),
                credential,
                role: PeerRole::Initiator,
                class: NetworkClass::Lan,
            },
            crate::link::ReverseCodec,
        )
        .unwrap();
    let (port, _receiver) = link.create_port().unwrap();
    let (binding, _) = link
        .bind_receive(association, &[(port, DeliveryTag::from_raw(1))])
        .unwrap();
    let (adjacent, _) = link
        .bind_receive(association, &[(port, DeliveryTag::from_raw(1))])
        .unwrap();
    let lifetime = || link.receive_port_cutoff(binding, port).unwrap();
    let window = pulsebeam_core::fabric::wan::Window::new(
        Profile::Opus,
        1 << 32,
        0,
        std::num::NonZeroU32::new(0x0100_0000).unwrap(),
        0,
    )
    .unwrap();
    let child = LiveNetworkChild {
        ingress: FabricIngress {
            association,
            key: NodeRouteKey::from_bytes([1, 0, 0, 0]),
            purpose: pulsebeam_link::FabricPurpose::Live,
        },
        destination,
        publication,
        incarnation: std::num::NonZeroU64::MIN,
        major: pulsebeam_core::fabric::MAJOR,
        revision: 0,
        sequence_anchor: 0,
        emission: Emission::new(Profile::Opus, &[]).unwrap(),
        window: Some(window),
    };
    let (mut writer, rx) = new_shard_update(shard);
    let mut updates = ShardUpdateApplication::new(shard, rx);
    writer.stage(
        1,
        ShardUpdateOp::InstallRoute {
            address: destination,
            action: TrackRouteAction::Forward {
                publication_id: publication,
            },
        },
    );
    let mut without_plan = writer.stage_live_network_child(1, child, lifetime());
    writer.publish().unwrap();
    assert_eq!(updates.apply(&mut execution, 1), 1);
    assert_eq!(without_plan.try_recv(), Ok(None));

    writer.stage_plans(
        2,
        vec![TrackPlanUpdate {
            publication_id: publication,
            plan: Some(TrackPlan::new([receiver], [], None)),
        }],
    );
    let mut installed = writer.stage_live_network_child(2, child, lifetime());
    let repair_ingress = FabricIngress {
        purpose: pulsebeam_link::FabricPurpose::RepairOnly,
        ..child.ingress
    };
    let mut wrong_purpose = writer.stage_live_network_child(
        2,
        LiveNetworkChild {
            ingress: repair_ingress,
            ..child
        },
        lifetime(),
    );
    let mut conflicting = writer.stage_live_network_child(
        2,
        LiveNetworkChild {
            incarnation: std::num::NonZeroU64::new(2).unwrap(),
            ..child
        },
        lifetime(),
    );
    writer.publish().unwrap();
    assert_eq!(updates.apply(&mut execution, 1), 1);
    assert_eq!(installed.try_recv(), Ok(Some(child)));
    assert_eq!(wrong_purpose.try_recv(), Ok(None));
    assert_eq!(conflicting.try_recv(), Ok(None));
    let packet = || {
        FabricPacket::new(
            Header {
                profile: Profile::Opus,
                marker: false,
                sequence: 0x7fff,
                timestamp: 0,
                ingress_ntp32: 0,
            },
            &[],
            Box::<[u8]>::from([1]),
        )
        .unwrap()
    };
    let position = (1 << 32) | 0x7fff;
    let image = || NetworkPacket {
        window,
        position,
        packet: packet(),
    };
    for wrong in [NetworkPacket {
        window,
        position: 1,
        packet: packet(),
    }] {
        assert_eq!(
            execution
                .runtime
                .offer_live_network(destination, child.ingress, now, now, wrong),
            Err(crate::route::fabric::local::Rejection::Binding)
        );
    }
    assert!(
        execution
            .runtime
            .offer_live_network(destination, child.ingress, now, now, image())
            .is_ok()
    );
    let source = execution
        .runtime
        .network_fabric_source(destination)
        .unwrap();
    assert_eq!(
        execution.runtime.fabric_sequence_anchor(source, now),
        Some(position)
    );
    assert_eq!(
        execution
            .runtime
            .offer_live_network(destination, repair_ingress, now, now, image()),
        Err(crate::route::fabric::local::Rejection::Binding)
    );
    let next_ssrc = std::num::NonZeroU32::new(0x0200_0000).unwrap();
    let next_start = window.start() + pulsebeam_core::fabric::wan::WINDOW_SPAN;
    let next_window =
        pulsebeam_core::fabric::wan::Window::new(Profile::Opus, next_start, 0, next_ssrc, 0)
            .unwrap();
    let next = LiveNetworkChild {
        ingress: FabricIngress {
            key: NodeRouteKey::from_bytes(next_ssrc.get().to_be_bytes()),
            ..child.ingress
        },
        window: Some(next_window),
        ..child
    };
    let next_image = |position, sequence| NetworkPacket {
        window: next_window,
        position,
        packet: FabricPacket::new(
            Header {
                sequence,
                ..packet().header()
            },
            &[],
            Box::<[u8]>::from([2]),
        )
        .unwrap(),
    };
    let mut receipt =
        writer.stage_live_network_child(3, next, link.receive_port_cutoff(adjacent, port).unwrap());
    assert_eq!(
        execution.runtime.offer_live_network(
            destination,
            next.ingress,
            now,
            now,
            next_image(next_start, 0x8000)
        ),
        Err(crate::route::fabric::local::Rejection::Binding)
    );
    writer.publish().unwrap();
    assert_eq!(updates.apply(&mut execution, 1), 1);
    assert_eq!(receipt.try_recv(), Ok(Some(next)));
    assert!(
        execution
            .runtime
            .offer_live_network(
                destination,
                next.ingress,
                now,
                now,
                next_image(next_start, 0x8000)
            )
            .is_ok()
    );
    assert_eq!(
        execution.runtime.fabric_sequence_anchor(source, now),
        Some(next_start)
    );
    lifetime().fence();
    assert!(
        execution
            .runtime
            .offer_live_network(
                destination,
                next.ingress,
                now,
                now,
                next_image(next_start + 1, 0x8001)
            )
            .is_ok()
    );
    assert_eq!(
        execution
            .runtime
            .offer_live_network(destination, child.ingress, now, now, image()),
        Err(crate::route::fabric::local::Rejection::Binding)
    );

    writer.stage(
        4,
        ShardUpdateOp::RetireRoute {
            address: destination,
        },
    );
    let mut delayed = writer.stage_live_network_child(4, child, lifetime());
    writer.publish().unwrap();
    assert_eq!(updates.apply(&mut execution, 1), 1);
    assert_eq!(delayed.try_recv(), Ok(None));
}

#[cfg(feature = "transport-conformance")]
struct CrossShardRtpFixture {
    origin: ShardExecution,
    clock_origin: Instant,
    mailbox: Option<CrossShardRtpMailbox>,
}

#[cfg(feature = "transport-conformance")]
struct CrossShardRtpMailbox {
    binding: crate::route::fabric::ShardBinding,
    _links: pulsebeam_link::LinkControl<ShardFrame>,
    rx: pulsebeam_link::LinkRx<ShardFrame>,
}

#[derive(Clone, Copy)]
struct FixtureRtpBinding {
    track: crate::keys::TrackHandle,
    source: FabricSourceHandle,
    branch: FabricLocalBranch,
}

pub(crate) struct FabricRtpFixture {
    execution: ShardExecution,
    #[cfg(feature = "transport-conformance")]
    cross_shard: Option<CrossShardRtpFixture>,
    publisher: ParticipantHandle,
    receiver: ParticipantHandle,
    emission: Emission,
    binding: Option<FixtureRtpBinding>,
    target: Option<pulsebeam_core::dd::evidence::DecodeTarget>,
}

impl FabricRtpFixture {
    pub(crate) fn new(
        publisher: ConnectionConfig,
        receiver: ConnectionConfig,
        profile: Profile,
        refresh_ssrc: Option<std::num::NonZeroU32>,
    ) -> Self {
        Self::with_selection(publisher, receiver, profile, refresh_ssrc, None)
    }

    pub(crate) fn with_selection(
        publisher: ConnectionConfig,
        receiver: ConnectionConfig,
        profile: Profile,
        refresh_ssrc: Option<std::num::NonZeroU32>,
        target: Option<pulsebeam_core::dd::evidence::DecodeTarget>,
    ) -> Self {
        let now = Instant::now();
        let mut execution = ShardExecution::new(
            ShardId::new(0),
            8,
            1,
            WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now),
        );
        let publisher_id = publisher.connection_id;
        let receiver_id = receiver.connection_id;
        let address =
            |slot| NodeTransportAddress::new(TransportRoute::new(ShardId::new(0), slot), 1);
        assert!(execution.add_participant(address(1), publisher));
        assert!(execution.add_participant(address(2), receiver));
        let publisher = execution.registry.resolve_connection(publisher_id).unwrap();
        let receiver = execution.registry.resolve_connection(receiver_id).unwrap();
        let kind = match profile {
            Profile::Opus => TrackKind::Audio,
            Profile::Data => TrackKind::Data,
            Profile::H264 | Profile::Av1 => TrackKind::Video,
        };
        let publication_id = publisher_id.fixture_publication(kind, "fixture");
        let track = execution
            .runtime
            .install_track(publication_id, None, Some(publisher), None);
        execution.apply_plan(&TrackPlanUpdate {
            publication_id,
            plan: Some(TrackPlan::new([receiver_id], [], None)),
        });
        let extensions = target.map(|_| ExtensionKind::DependencyDescriptor);
        let emission = Emission::new(profile, extensions.as_slice()).unwrap();
        let source = execution
            .install_fabric_source(track, publisher, emission, refresh_ssrc)
            .unwrap();
        let branch = match target {
            Some(target) => execution
                .install_fabric_filtered_branch(track, source, receiver, emission, target, now),
            None => execution.install_fabric_branch(track, source, receiver, emission),
        }
        .unwrap();
        Self {
            execution,
            #[cfg(feature = "transport-conformance")]
            cross_shard: None,
            publisher,
            receiver,
            emission,
            binding: Some(FixtureRtpBinding {
                track,
                source,
                branch,
            }),
            target,
        }
    }

    fn binding(&self) -> FixtureRtpBinding {
        self.binding.expect("fixture publication is routed")
    }

    #[cfg(feature = "transport-conformance")]
    pub(crate) fn across_shards(
        publisher: ConnectionConfig,
        receiver: ConnectionConfig,
        profile: Profile,
        now: Instant,
    ) -> Self {
        let origin_id = ShardId::new(1);
        let receiver_shard = ShardId::new(0);
        let anchor = WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now);
        let mut origin = ShardExecution::new(origin_id, 8, 2, anchor);
        let mut execution = ShardExecution::new(receiver_shard, 8, 2, anchor);
        let publisher_id = publisher.connection_id;
        let receiver_id = receiver.connection_id;
        assert!(origin.add_participant(
            NodeTransportAddress::new(TransportRoute::new(origin_id, 1), 1),
            publisher,
        ));
        assert!(execution.add_participant(
            NodeTransportAddress::new(TransportRoute::new(receiver_shard, 1), 1),
            receiver,
        ));
        let publisher = origin.registry.resolve_connection(publisher_id).unwrap();
        let receiver = execution.registry.resolve_connection(receiver_id).unwrap();
        Self {
            execution,
            cross_shard: Some(CrossShardRtpFixture {
                origin,
                clock_origin: now,
                mailbox: None,
            }),
            publisher,
            receiver,
            emission: Emission::new(profile, &[]).unwrap(),
            binding: None,
            target: None,
        }
    }

    #[cfg(feature = "transport-conformance")]
    fn install_cross_shard_publication(
        &mut self,
        publication: crate::entity::PublicationKey,
        refresh_ssrc: Option<std::num::NonZeroU32>,
    ) {
        use crate::route::{NodeRouteAddress, RouteId, fabric::ShardBinding};
        use crate::shard_update::CompiledRoute;
        use pulsebeam_link::{Destination, LinkControl, LinkTime};

        assert!(self.binding.is_none());
        let publisher_id = self.publisher().connection_id;
        let receiver_id = self.receiver().connection_id;
        let (publisher, receiver, emission) = (self.publisher, self.receiver, self.emission);
        let execution = &mut self.execution;
        let cross = self.cross_shard.as_mut().unwrap();
        let origin = &mut cross.origin;
        let source_track = origin
            .runtime
            .install_track(publication, None, Some(publisher), None);
        assert!(origin.apply_participant_effect(
            publisher_id,
            crate::participant::ParticipantEffect::TrackPublished {
                publication_id: publication
            },
        ));
        let source = origin
            .install_fabric_source(source_track, publisher, emission, refresh_ssrc)
            .unwrap();
        let address = NodeRouteAddress::new(RouteId::new(execution.shard_id, 1), 1);
        let binding = ShardBinding {
            destination: address,
            publication,
            origin: origin.shard_id,
            incarnation: origin.runtime.fabric_incarnation(source).unwrap(),
        };
        let mut links = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
        let (port, mailbox) = links.create_port().unwrap();
        let (_, tx) = links
            .bind_send(Destination::Local {
                port,
                tag: crate::link::delivery_tag(address),
            })
            .unwrap();
        let mut plan = TrackPlan::new([], [address], None);
        plan.compiled_remote.push(CompiledRoute {
            address,
            tx,
            clock_origin: cross.clock_origin,
        });
        origin.apply_plan(&TrackPlanUpdate {
            publication_id: publication,
            plan: Some(plan),
        });
        origin.apply_lifecycle_op(&ShardUpdateOp::BindShardFabricDestination { binding });

        let track = execution
            .runtime
            .install_track(publication, None, None, None);
        execution.apply_plan(&TrackPlanUpdate {
            publication_id: publication,
            plan: Some(TrackPlan::new([receiver_id], [], None)),
        });
        execution.apply_lifecycle_op(&ShardUpdateOp::InstallRoute {
            address,
            action: crate::shard_update::TrackRouteAction::Forward {
                publication_id: publication,
            },
        });
        execution.apply_lifecycle_op(&ShardUpdateOp::InstallShardFabricSource {
            binding,
            encoding: None,
            emission,
        });
        let imported = execution.runtime.shard_fabric_source(binding).unwrap();
        let branch = execution
            .install_fabric_branch(track, imported, receiver, emission)
            .unwrap();
        cross.mailbox = Some(CrossShardRtpMailbox {
            binding,
            _links: links,
            rx: mailbox,
        });
        self.binding = Some(FixtureRtpBinding {
            track,
            source,
            branch,
        });
    }

    fn publisher_execution(&mut self) -> &mut ShardExecution {
        #[cfg(feature = "transport-conformance")]
        if let Some(cross) = &mut self.cross_shard {
            return &mut cross.origin;
        }
        &mut self.execution
    }

    #[cfg(feature = "transport-conformance")]
    pub(crate) fn deliver_shard_fabric(
        &mut self,
        now: Instant,
        mut inspect: impl FnMut(&OwnedPacket),
    ) -> usize {
        use crate::shard::worker::bind_received_frame;

        struct IngressOnly;
        impl IngressTransport for IngressOnly {
            fn send_frame(&self, _: ShardId, _: ShardFrame) {
                panic!("AV1 forward witness did not configure a reverse route");
            }
        }
        let cross = self.cross_shard.as_mut().unwrap();
        let mailbox = cross.mailbox.as_mut().unwrap();
        let mut batch = Vec::new();
        let received = mailbox.rx.try_recv_batch(&mut batch, 64);
        for mut frame in batch {
            let ShardFrame::Fabric(ref offer) = frame.packet else {
                panic!("expected typed Fabric offer")
            };
            assert_eq!(offer.binding, mailbox.binding);
            assert_eq!(offer.binding.origin, cross.origin.shard_id);
            assert_ne!(offer.binding.origin, self.execution.shard_id);
            inspect(&offer.packet);
            assert!(bind_received_frame(self.execution.shard_id, &mut frame));
            self.execution
                .on_shard_frames([frame.packet], now, &IngressOnly);
        }
        received
    }

    pub(crate) fn install_peer_publisher_probe(
        &mut self,
        publisher: crate::route::fabric::DataPublisher,
        now: Instant,
    ) -> (
        crate::link::wan_host::SendFixture,
        crate::route::fabric::CompiledPublisherSend,
    ) {
        use crate::route::fabric::{CompiledPublisherSend, LiveNextHop, PublisherSendChange};
        use pulsebeam_core::fabric::wan::Window;
        let ssrc = std::num::NonZeroU32::MIN;
        let window = Window::new(Profile::Data, 0, 0, ssrc, 0).unwrap();
        let expires_at = now + std::time::Duration::from_secs(2);
        let (fixture, peer, tx) = crate::link::wan_host::SendFixture::new(window, expires_at);
        let lifetime = tx.cutoff();
        let route = CompiledPublisherSend {
            publisher: publisher.into(),
            shard: self.execution.shard_id,
            next: LiveNextHop {
                peer,
                key: pulsebeam_link::NodeRouteKey::from_bytes(ssrc.get().to_be_bytes()),
            },
            expires_at,
            tx: tx,
        };
        assert!(
            self.execution
                .update_publisher_send(PublisherSendChange::Install {
                    route: route.clone(),
                    lifetime
                })
        );
        (fixture, route)
    }

    pub(crate) fn probe_peer_publisher_lookup(
        &mut self,
        route: crate::route::fabric::CompiledPublisherSend,
        unit: crate::route::fabric::local::DataExpectation,
        request: crate::route::fabric::local::RepairRequest,
    ) -> pulsebeam_runtime::sync::oneshot::Receiver<
        Option<crate::route::fabric::relay::LocatedRepair>,
    > {
        use crate::route::fabric::{
            PublisherObservation,
            local::PropagationOutcome,
            relay::{PeerRepairLookup, PublisherRepairLookup},
        };
        self.execution
            .observe_publisher_data_expectation(PublisherObservation {
                route: route.clone(),
                unit,
                outcome: PropagationOutcome::Confirmed,
            });
        let (mut writer, reader) = crate::shard_update::new_shard_update(self.execution.shard_id);
        let reply = writer.stage_relay_repair_lookup(
            1,
            PeerRepairLookup::Publisher(PublisherRepairLookup {
                route,
                request,
                branch: std::num::NonZeroU64::MIN,
            }),
        );
        writer.publish().unwrap();
        let mut updates =
            crate::shard::updates::ShardUpdateApplication::new(self.execution.shard_id, reader);
        assert_eq!(updates.apply(&mut self.execution, 1), 1);
        reply
    }

    pub(crate) fn sample_data_publisher_anchor(
        &mut self,
        publisher: crate::route::fabric::DataPublisher,
    ) -> Option<u64> {
        let shard = self.execution.shard_id;
        let (mut writer, reader) = crate::shard_update::new_shard_update(shard);
        let mut receipt = writer.stage_publisher_anchor(1, publisher.into());
        writer.publish().unwrap();
        let mut updates = crate::shard::updates::ShardUpdateApplication::new(shard, reader);
        assert_eq!(updates.apply(&mut self.execution, 1), 1);
        receipt.try_recv().unwrap()
    }

    #[cfg(feature = "transport-conformance")]
    pub(crate) fn publisher_shard(&mut self) -> ShardId {
        self.publisher_execution().shard_id
    }

    pub(crate) fn publisher(&mut self) -> &mut crate::participant::Connection {
        let publisher = self.publisher;
        self.publisher_execution()
            .registry
            .resolve_mut(publisher)
            .unwrap()
    }

    pub(crate) fn receiver(&mut self) -> &mut crate::participant::Connection {
        self.execution.registry.resolve_mut(self.receiver).unwrap()
    }

    pub(crate) fn use_negotiated_publication(
        &mut self,
        publication: crate::entity::PublicationKey,
        refresh_ssrc: Option<std::num::NonZeroU32>,
    ) {
        #[cfg(feature = "transport-conformance")]
        if self.cross_shard.is_some() {
            self.install_cross_shard_publication(publication, refresh_ssrc);
            return;
        }
        let old = self
            .execution
            .runtime
            .publication_id(self.binding().track)
            .unwrap();
        self.execution.apply_plan(&TrackPlanUpdate {
            publication_id: old,
            plan: None,
        });
        self.execution
            .apply_lifecycle_op(&ShardUpdateOp::RemoveTrackRuntime {
                publication_id: old,
            });
        let track =
            self.execution
                .runtime
                .install_track(publication, None, Some(self.publisher), None);
        let publisher = self.publisher().connection_id;
        let receiver = self.receiver().connection_id;
        assert!(self.execution.apply_participant_effect(
            publisher,
            crate::participant::ParticipantEffect::TrackPublished {
                publication_id: publication
            }
        ));
        self.execution.apply_plan(&TrackPlanUpdate {
            publication_id: publication,
            plan: Some(TrackPlan::new([receiver], [], None)),
        });
        let source = self
            .execution
            .install_fabric_source(track, self.publisher, self.emission, refresh_ssrc)
            .unwrap();
        let branch = self
            .execution
            .install_fabric_branch(track, source, self.receiver, self.emission)
            .unwrap();
        self.binding = Some(FixtureRtpBinding {
            track,
            source,
            branch,
        });
    }

    pub(crate) fn attach_publisher_ingress(
        &mut self,
        ingress: crate::rtp::fabric_ingress::RtpIngressShadow,
    ) -> bool {
        let (source, publisher) = (self.binding().source, self.publisher);
        self.publisher_execution()
            .attach_fabric_ingress(source, publisher, ingress)
    }

    pub(crate) fn attach_data(
        &mut self,
        parameters: pulsebeam_proto::state::SctpDataV1Parameters,
        publish: pulsebeam_rtc::DataChannelInstallation,
        subscribe: pulsebeam_rtc::DataChannelInstallation,
    ) {
        use crate::participant::data_fabric::PreparedSctp;
        use pulsebeam_proto::data_control::SubscriberBinding;
        use pulsebeam_proto::data_endpoint::Endpoint;

        let prepared =
            |installation| PreparedSctp::from_negotiation(parameters, installation).unwrap();
        let FixtureRtpBinding {
            track,
            source: old_source,
            ..
        } = self.binding();
        assert!(self.execution.runtime.retire_fabric_source(old_source));
        assert!(
            self.execution
                .stage_fabric_data_publisher(track, self.publisher, prepared(subscribe), None)
                .is_none()
        );
        let (handle, source) = self
            .execution
            .stage_fabric_data_publisher(track, self.publisher, prepared(publish), None)
            .unwrap();
        let branch = self
            .execution
            .install_fabric_branch(track, handle, self.receiver, self.emission)
            .unwrap();
        self.binding = Some(FixtureRtpBinding {
            track,
            source: handle,
            branch,
        });
        assert!(self.execution.attach_fabric_data_egress(
            branch,
            self.receiver,
            prepared(subscribe).bind(Endpoint::Subscriber(SubscriberBinding {
                source,
                branch: std::num::NonZeroU64::MIN,
            })),
            None,
        ));
    }

    pub(crate) fn bind_publisher_mailbox(
        &mut self,
        now: Instant,
    ) -> (
        pulsebeam_link::LinkControl<ShardFrame>,
        pulsebeam_link::LinkRx<ShardFrame>,
    ) {
        use crate::route::{NodeRouteAddress, RouteId, fabric::ShardBinding};
        use crate::shard_update::CompiledRoute;
        let receiver = self.receiver().connection_id;
        let publication = self
            .execution
            .runtime
            .publication_id(self.binding().track)
            .unwrap();
        let address = NodeRouteAddress::new(RouteId::new(ShardId::new(1), 2), 1);
        let mut links =
            pulsebeam_link::LinkControl::new(|| pulsebeam_link::LinkTime::from_nanos(0)).unwrap();
        let (port, rx) = links.create_port().unwrap();
        let (_, tx) = links
            .bind_send(pulsebeam_link::Destination::Local {
                port,
                tag: crate::link::delivery_tag(address),
            })
            .unwrap();
        let mut plan = TrackPlan::new([receiver], [address], None);
        plan.compiled_remote.push(CompiledRoute {
            address,
            tx,
            clock_origin: now,
        });
        self.execution.apply_plan(&TrackPlanUpdate {
            publication_id: publication,
            plan: Some(plan),
        });
        let binding = ShardBinding {
            destination: address,
            publication,
            origin: self.execution.shard_id,
            incarnation: self
                .execution
                .runtime
                .fabric_incarnation(self.binding().source)
                .unwrap(),
        };
        self.execution
            .apply_lifecycle_op(&ShardUpdateOp::BindShardFabricDestination { binding });
        (links, rx)
    }

    pub(crate) fn flush_publisher_media(&mut self, now: Instant) -> usize {
        self.publisher_execution().flush_stream_buffers(now, 64)
    }

    pub(crate) fn fire_fabric_control_timers(&mut self, now: Instant) {
        self.execution
            .fire_timers(now, |_, _| panic!("unexpected shard frame"));
    }

    pub(crate) fn has_fabric_control_work(&self) -> bool {
        self.execution
            .runtime
            .next_fabric_data_propagation()
            .is_some()
    }

    pub(crate) fn publisher_with_sink(
        &mut self,
    ) -> (
        &mut crate::participant::Connection,
        crate::shard::events::PipelineSinkRef<'_>,
    ) {
        let publisher = self.publisher;
        Self::participant_with_sink(self.publisher_execution(), publisher)
    }

    pub(crate) fn receiver_with_sink(
        &mut self,
    ) -> (
        &mut crate::participant::Connection,
        crate::shard::events::PipelineSinkRef<'_>,
    ) {
        Self::participant_with_sink(&mut self.execution, self.receiver)
    }

    fn participant_with_sink(
        execution: &mut ShardExecution,
        handle: ParticipantHandle,
    ) -> (
        &mut crate::participant::Connection,
        crate::shard::events::PipelineSinkRef<'_>,
    ) {
        let receiver = execution.registry.resolve_mut(handle).unwrap();
        let sink = execution.pipeline.polling_sink(
            crate::shard::events::ParticipantBinding {
                participant_id: receiver.participant_id,
                connection_id: receiver.connection_id,
                handle,
            },
            &mut execution.runtime,
        );
        (receiver, sink)
    }

    pub(crate) fn take_published_track(
        &mut self,
        publication: crate::entity::PublicationKey,
    ) -> Option<crate::track::Track> {
        while let Some(event) = self.execution.pipeline.pop_shard_event() {
            if let ShardEvent::TrackPublished(track) = event
                && track.id() == publication
            {
                return Some(track);
            }
        }
        None
    }

    pub(crate) fn apply_data_subscription_plan(
        &mut self,
        track: &crate::track::Track,
        reference: u64,
    ) {
        use crate::participant::ParticipantEffect;
        let connection = self.receiver().connection_id;
        let subscriber = self.receiver().participant_id;
        assert!(std::iter::from_fn(|| self.execution.pipeline.pop_shard_event()).any(|event| matches!(event,
            ShardEvent::OwnedSubscriptionSet { connection_id, request }
            if connection_id == connection && request.reference == reference && request.track == track.id()
                && request.source_room == track.meta().room && request.subscriber == subscriber && request.enabled)));
        assert!(self.execution.apply_participant_effect(
            connection,
            ParticipantEffect::TrackCandidateAdded {
                track: Box::new(track.clone()),
            }
        ));
        self.execution.apply_plan(&TrackPlanUpdate {
            publication_id: track.id(),
            plan: Some(TrackPlan::new([connection], [], None)),
        });
        assert!(self.execution.apply_participant_effect(
            connection,
            ParticipantEffect::TrackSubscribed {
                publication_id: track.id()
            }
        ));
    }

    pub(crate) fn flush_receiver_feedback(&mut self) {
        self.flush_receiver_feedback_with_clock(Instant::now);
    }

    pub(crate) fn flush_receiver_feedback_with_clock(&mut self, now: impl Fn() -> Instant) {
        struct Local;
        impl IngressTransport for Local {
            fn send_frame(&self, _: ShardId, _: ShardFrame) {
                panic!("fixture must stay on one shard");
            }
        }
        self.execution
            .flush_participant_events_with_clock(&Local, 64, now);
    }

    pub(crate) fn offer(
        &mut self,
        now: Instant,
        arrival: Instant,
        sequence: u64,
        packet: OwnedPacket,
    ) {
        assert_eq!(
            self.offer_report(now, arrival, sequence, packet).enqueued,
            1
        );
    }

    pub(crate) fn offer_report(
        &mut self,
        now: Instant,
        arrival: Instant,
        sequence: u64,
        packet: OwnedPacket,
    ) -> Offers {
        self.execution
            .offer_fabric(
                self.binding().source,
                self.publisher,
                now,
                arrival,
                sequence,
                packet,
            )
            .unwrap()
    }

    pub(crate) fn bind_output(
        &mut self,
        leg: crate::rtp::fabric_egress::SenderLeg<FabricLocalBranch>,
        metadata: crate::rtp::fabric_egress::SourceMetadata,
    ) -> bool {
        let branch = self.binding().branch;
        #[cfg(feature = "transport-conformance")]
        if let Some(cross) = &self.cross_shard {
            // Controlled AV1 fixture only: bypass production codec admission,
            // not the receiving shard's live branch and owner checks.
            let expected = cross.mailbox.as_ref().unwrap().binding;
            return matches!(self.execution.runtime.fabric_handoff_owner(branch, self.receiver),
                Some(crate::shard::router::FabricOwner::Shard(binding)) if binding == expected)
                && self
                    .execution
                    .registry
                    .resolve_mut(self.receiver)
                    .unwrap()
                    .attach_fabric_rtp_output(branch, leg, metadata);
        }
        self.execution
            .attach_fabric_rtp_egress(branch, self.receiver, leg, metadata)
    }

    pub(crate) fn write(
        &mut self,
        now: Instant,
        leg: &mut crate::rtp::fabric_egress::SenderLeg<FabricLocalBranch>,
        metadata: crate::rtp::fabric_egress::SourceMetadata,
    ) {
        assert_eq!(self.try_write(now, leg, metadata), Ok(Some(())));
    }

    pub(crate) fn try_write(
        &mut self,
        now: Instant,
        leg: &mut crate::rtp::fabric_egress::SenderLeg<FabricLocalBranch>,
        metadata: crate::rtp::fabric_egress::SourceMetadata,
    ) -> Result<Option<()>, crate::rtp::fabric_egress::AdmissionError> {
        self.execution
            .write_fabric_rtp(self.binding().branch, self.receiver, now, leg, metadata)
    }

    pub(crate) fn receiver_handoff_staged(&mut self) -> bool {
        self.execution
            .registry
            .resolve_mut(self.receiver)
            .unwrap()
            .staged_handoff(self.receiver)
            .is_some()
    }

    pub(crate) fn queue_receiver_datagrams(&mut self) {
        let execution = &mut self.execution;
        execution.dispatch_staged_media(self.receiver);
        let mut egress = ShardNetworkEgress {
            udp: &mut execution.udp_send_batch,
        };
        let receiver = execution.registry.resolve_mut(self.receiver).unwrap();
        receiver.drain_network(&mut egress);
        assert!(!receiver.network_pending());
    }

    #[cfg(feature = "transport-conformance")]
    pub(crate) fn flush_receiver_udp(&mut self, socket: &mut pulsebeam_udp::UdpSocket) -> bool {
        self.queue_receiver_datagrams();
        self.execution.flush_udp(socket)
    }

    pub(crate) fn submit_receiver(
        &mut self,
        send: impl FnOnce(&[pulsebeam_udp::SendDatagram<'_>]) -> usize,
    ) {
        self.queue_receiver_datagrams();
        let execution = &mut self.execution;
        let submission = execution.udp_send_batch.submit(
            |handoff| {
                ShardExecution::authorize_media_handoff(
                    &execution.runtime,
                    &mut execution.registry,
                    handoff,
                )
            },
            send,
        );
        execution.resolve_media_submissions(submission.handoffs);
    }

    pub(crate) fn reset_output(&mut self) {
        self.execution
            .reset_fabric_output(self.receiver, self.binding().branch);
    }

    pub(crate) fn replace_source(&mut self) -> FabricLocalBranch {
        let FixtureRtpBinding {
            track,
            source: old_source,
            ..
        } = self.binding();
        assert!(self.execution.runtime.retire_fabric_source(old_source));
        let source = self
            .execution
            .install_fabric_source(track, self.publisher, self.emission, None)
            .unwrap();
        let branch = match self.target {
            Some(target) => self.execution.install_fabric_filtered_branch(
                track,
                source,
                self.receiver,
                self.emission,
                target,
                Instant::now(),
            ),
            None => {
                self.execution
                    .install_fabric_branch(track, source, self.receiver, self.emission)
            }
        }
        .unwrap();
        self.binding = Some(FixtureRtpBinding {
            track,
            source,
            branch,
        });
        branch
    }
}

fn participant(
    execution: &mut ShardExecution,
    slot: u32,
    name: &str,
) -> (ConnectionId, NodeTransportAddress, ParticipantHandle) {
    let connection_id = ConnectionId::new();
    let address = NodeTransportAddress::new(TransportRoute::new(execution.shard_id, slot), 1);
    assert!(execution.add_participant(
        address,
        ConnectionConfig {
            scope: ProjectRoomKey::fixture(RoomId::new("fabric-candidate").unwrap()).into(),
            participant_id: ParticipantId::new(name).unwrap(),
            connection_id,
            profile: SignalingProtocol::State,
            authorization_expiry: None,
            rtc: crate::control::application_only_test_connection(),
            resources: NegotiatedResources::empty_for_test(),
        }
    ));
    (
        connection_id,
        address,
        execution
            .registry
            .resolve_connection(connection_id)
            .unwrap(),
    )
}

fn packet(sequence: u32) -> OwnedPacket {
    FabricPacket::new(
        Header {
            profile: Profile::Opus,
            marker: false,
            sequence,
            timestamp: 24,
            ingress_ntp32: 64,
        },
        &[],
        Box::from(&b"opaque"[..]),
    )
    .unwrap()
}

#[test]
fn opus_anchor_samples_only_the_exact_live_publisher_emission() {
    use crate::shard_update::PublisherAnchor;
    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let now = Instant::now();
    let mut execution = ShardExecution::new(
        ShardId::new(0),
        8,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now),
    );
    let (connection, _, publisher) = participant(&mut execution, 1, "publisher");
    let (other, _, _) = participant(&mut execution, 2, "not-publisher");
    let publication = connection.fixture_publication(TrackKind::Audio, "opus-anchor");
    let track = execution
        .runtime
        .install_track(publication, None, Some(publisher), None);
    let emission = Emission::new(Profile::Opus, &[]).unwrap();
    let source = execution
        .install_fabric_source(track, publisher, emission, None)
        .unwrap();
    let incarnation = execution.runtime.fabric_incarnation(source).unwrap();
    let query = || PublisherAnchor {
        publication,
        connection,
        incarnation,
        emission,
    };
    assert_eq!(execution.sample_publisher_anchor(query(), now), None);

    let sequence = u64::from(u32::MAX) + 2;
    execution
        .offer_fabric(source, publisher, now, now, sequence, packet(1))
        .unwrap();
    assert_eq!(
        execution.sample_publisher_anchor(query(), now),
        Some(sequence)
    );
    assert_eq!(
        execution.sample_publisher_anchor(
            PublisherAnchor {
                connection: other,
                ..query()
            },
            now
        ),
        None
    );
    assert_eq!(
        execution.sample_publisher_anchor(
            PublisherAnchor {
                incarnation: std::num::NonZeroU64::new(incarnation.get() + 1).unwrap(),
                ..query()
            },
            now
        ),
        None
    );
    assert_eq!(
        execution.sample_publisher_anchor(
            PublisherAnchor {
                emission: Emission::new(Profile::Opus, &[ExtensionKind::AudioLevel]).unwrap(),
                ..query()
            },
            now
        ),
        None
    );
    assert_eq!(
        execution.sample_publisher_anchor(query(), now + std::time::Duration::from_secs(1)),
        None
    );
    execution
        .runtime
        .fence_local_fabric_source(source, publisher);
    assert_eq!(execution.sample_publisher_anchor(query(), now), None);
}

#[test]
fn shard_data_notice_admits_only_current_exact_receivers() {
    use crate::route::fabric::local::{
        DataExpectation, ExpectationAdmission, ExpectationWrite, PropagationStep,
    };
    use crate::route::{
        NodeRouteAddress, RouteId,
        fabric::{ShardBinding, ShardDataExpectation, ShardDataReceiver},
    };
    use crate::shard::worker::{ShardFrame, bind_received_frame, send_link_frame};
    use pulsebeam_link::{Destination, LinkControl, LinkTime};
    use std::num::NonZeroU64;

    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let now = Instant::now();
    let shard = ShardId::new(0);
    let origin = ShardId::new(1);
    let mut execution = ShardExecution::new(
        shard,
        8,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now),
    );
    let (receiver_id, _, receiver_handle) = participant(&mut execution, 1, "current");
    let (replacement_id, _, replacement) = participant(&mut execution, 2, "replaced");
    let publication = receiver_id.fixture_publication(TrackKind::Data, "remote-ordered");
    let track = execution
        .runtime
        .install_track(publication, None, None, None);
    execution.apply_plan(&TrackPlanUpdate {
        publication_id: publication,
        plan: Some(TrackPlan::new([receiver_id, replacement_id], [], None)),
    });
    let address = NodeRouteAddress::new(RouteId::new(shard, 3), 1);
    let binding = ShardBinding {
        destination: address,
        publication,
        origin,
        incarnation: NonZeroU64::MIN,
    };
    execution.apply_lifecycle_op(&ShardUpdateOp::InstallRoute {
        address,
        action: crate::shard_update::TrackRouteAction::Forward {
            publication_id: publication,
        },
    });
    execution.apply_lifecycle_op(&ShardUpdateOp::InstallShardFabricSource {
        binding,
        encoding: None,
        emission: Emission::DATA,
    });
    let source = execution.runtime.shard_fabric_source(binding).unwrap();
    let branch = execution
        .install_fabric_branch(track, source, receiver_handle, Emission::DATA)
        .unwrap();
    execution
        .install_fabric_branch(track, source, replacement, Emission::DATA)
        .unwrap();

    let mut links = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
    let (port, mut rx) = links.create_port().unwrap();
    let bind = |links: &mut LinkControl<ShardFrame>, address| {
        links
            .bind_send(Destination::Local {
                port,
                tag: crate::link::delivery_tag(address),
            })
            .unwrap()
            .1
    };
    let tx = bind(&mut links, address);
    let wrong = bind(&mut links, NodeRouteAddress::new(RouteId::new(shard, 4), 1));
    let receiver = ShardDataReceiver {
        connection: receiver_id,
        branch: NonZeroU64::MIN,
    };
    let replaced = ShardDataReceiver {
        connection: replacement_id,
        branch: NonZeroU64::MIN,
    };
    let unit = DataExpectation {
        unit_seq: 3,
        fragment_count: 2,
    };
    let expires = now + std::time::Duration::from_secs(1);
    let notice = ShardFrame::FabricDataExpectation(ShardDataExpectation {
        binding,
        unit,
        expires,
        receivers: Box::new([receiver, replaced]),
    });
    assert!(!send_link_frame(&wrong, now, notice.clone()));
    assert!(send_link_frame(&tx, now, notice));
    let mut frames = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut frames, 4), 1);
    let mut frame = frames.pop().unwrap();
    assert!(!bind_received_frame(origin, &mut frame));
    assert!(bind_received_frame(shard, &mut frame));
    let ShardFrame::FabricDataExpectation(mut notice) = frame.packet else {
        panic!("expected a DATA notice")
    };
    assert_eq!(
        execution
            .runtime
            .accept_shard_data_expectation(&notice, now, |_, member| {
                Some(if member == replacement {
                    ShardDataReceiver {
                        branch: NonZeroU64::MAX,
                        ..replaced
                    }
                } else {
                    receiver
                })
            }),
        Some(ExpectationAdmission::First)
    );
    notice.receivers[1].branch = NonZeroU64::MAX;
    assert_eq!(
        execution
            .runtime
            .accept_shard_data_expectation(&notice, now, |_, _| panic!(
                "replay must not widen cohort"
            )),
        Some(ExpectationAdmission::Replay)
    );
    assert_eq!(
        execution.runtime.tick_fabric_data_propagation(
            source,
            now,
            |candidate, _, expected, incarnation| {
                assert_eq!(
                    (candidate, expected, incarnation),
                    (branch, unit, receiver.branch)
                );
                ExpectationWrite::Queued
            }
        ),
        Some(PropagationStep::Attempted)
    );
    assert_eq!(
        execution
            .runtime
            .tick_fabric_data_propagation(source, now, |_, _, _, _| panic!("no second member")),
        None
    );
    assert!(execution.runtime.settle_fabric_data_propagation(
        receiver_handle,
        branch,
        unit,
        crate::route::fabric::local::PropagationOutcome::Confirmed,
        now
    ));
    use crate::route::fabric::{
        ShardDataRepair, ShardDataRepairImage,
        local::{RepairAdmission, RepairFragment, RepairQueue, RepairRequest},
    };
    let target = branch.repair_target(receiver_handle, receiver.branch);
    let request = RepairRequest {
        fragment: RepairFragment {
            unit_seq: unit.unit_seq,
            index: 1,
        },
        id: NonZeroU64::MIN,
    };
    assert_eq!(
        execution
            .runtime
            .repair_fanout(source)
            .unwrap()
            .request_data_repair(now, target, request),
        RepairAdmission::First
    );
    let packet = FabricPacket::new(
        Header {
            profile: Profile::Data,
            marker: true,
            sequence: 0,
            timestamp: 3,
            ingress_ntp32: 0,
        },
        &[],
        Box::<[u8]>::from(crate::participant::data_fabric::R4.as_slice()),
    )
    .unwrap();
    let image = ShardDataRepairImage {
        repair: ShardDataRepair {
            binding,
            receiver,
            request,
        },
        sequence: 0,
        packet,
    };
    assert_eq!(
        execution
            .runtime
            .repair_fanout(source)
            .unwrap()
            .enqueue_shard_repair(now, target, image),
        RepairQueue::Queued
    );
    assert!(
        execution
            .runtime
            .admit_fabric_data(
                branch,
                receiver_handle,
                now,
                Some(receiver.branch),
                |position, image, repair| {
                    assert_eq!((position, repair), (0, Some(request)));
                    assert_eq!(image.payload(), crate::participant::data_fabric::R4);
                    true
                }
            )
            .is_some_and(|handoff| handoff.completed == Some(request))
    );
    let lease = crate::participant::data_fabric::SubscriberLease {
        owned: 1,
        branch,
        endpoint: pulsebeam_proto::data_control::SubscriberBinding {
            source: binding.incarnation,
            branch: receiver.branch,
        },
    };
    use crate::route::fabric::local::ReceiptAcceptance;
    assert_eq!(
        execution
            .runtime
            .record_fabric_assembly_receipt(receiver_handle, lease, unit, now),
        Some(ReceiptAcceptance::First)
    );
    assert!(execution.runtime.next_fabric_delivery_report().is_some());
    let sent = std::cell::Cell::new(0usize);
    let send = |destination, frame| {
        assert_eq!(destination, origin);
        let ShardFrame::FabricDataReceipt(receipt) = frame else {
            panic!("expected receipt")
        };
        assert_eq!(
            (receipt.binding, receipt.unit, receipt.branch),
            (binding, unit, receiver.branch)
        );
        sent.set(sent.get() + 1);
    };
    execution.advance_fabric_control(now, send);
    execution.advance_fabric_control(now + std::time::Duration::from_millis(100), send);
    assert_eq!(sent.get(), 2);
    execution.advance_fabric_control(expires + std::time::Duration::from_millis(101), |_, _| {
        panic!("expired receipt")
    });
    assert!(execution.runtime.next_fabric_delivery_report().is_none());
    assert_eq!(
        execution
            .runtime
            .record_fabric_assembly_receipt(receiver_handle, lease, unit, now),
        Some(ReceiptAcceptance::Replay)
    );
    assert_eq!(
        execution
            .runtime
            .record_fabric_assembly_receipt(receiver_handle, lease, unit, expires),
        None
    );
    execution.apply_lifecycle_op(&ShardUpdateOp::RetireRoute { address });
    assert_eq!(
        execution
            .runtime
            .accept_shard_data_expectation(&notice, now, |_, _| panic!("retired route")),
        None
    );
}

#[test]
fn shard_mailbox_preserves_fabric_image_and_fences_rebound_sources() {
    use crate::route::{
        NodeRouteAddress, RouteId,
        fabric::{ShardBinding, ShardOffer},
    };
    use crate::shard::worker::{ShardFrame, bind_received_frame};
    use crate::shard_update::{CompiledRoute, TrackRouteAction};
    use pulsebeam_link::{Destination, LinkControl, LinkTime};
    use pulsebeam_runtime::time::Duration;

    struct Local;
    impl IngressTransport for Local {
        fn send_frame(&self, _: ShardId, _: ShardFrame) {
            panic!("a receiving shard must not re-originate the offer");
        }
    }

    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let now = Instant::now();
    let shard = ShardId::new(0);
    let mut execution = ShardExecution::new(
        shard,
        8,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now),
    );
    let (receiver_id, receiver_address, receiver) = participant(&mut execution, 1, "receiver");
    let origin = ShardId::new(1);
    let mut producer = ShardExecution::new(
        origin,
        8,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now),
    );
    let (publisher_id, _, publisher) = participant(&mut producer, 1, "publisher");
    let publication = publisher_id.fixture_publication(TrackKind::Video, "remote-fabric");
    let source_track = producer
        .runtime
        .install_track(publication, None, Some(publisher), None);
    let emission = Emission::new(Profile::H264, &[]).unwrap();
    let packet = |sequence| {
        let payload: Box<[u8]> = if sequence == 2 {
            let mut bytes = vec![0x65; 1188];
            bytes[1187] = 0xa5;
            bytes.into_boxed_slice()
        } else {
            Box::from(&b"opaque"[..])
        };
        FabricPacket::new(
            Header {
                profile: Profile::H264,
                marker: sequence == 2,
                sequence,
                timestamp: 24,
                ingress_ntp32: 64,
            },
            &[],
            payload,
        )
        .unwrap()
    };
    let source = producer
        .install_fabric_source(source_track, publisher, emission, None)
        .unwrap();
    let track = execution
        .runtime
        .install_track(publication, None, None, None);
    execution.apply_plan(&TrackPlanUpdate {
        publication_id: publication,
        plan: Some(TrackPlan::new([receiver_id], [], None)),
    });
    let address = NodeRouteAddress::new(RouteId::new(shard, 2), 1);
    let binding = ShardBinding {
        destination: address,
        publication,
        origin,
        incarnation: producer.runtime.fabric_incarnation(source).unwrap(),
    };
    let install =
        |execution: &mut ShardExecution, binding: ShardBinding, receiver: ParticipantHandle| {
            execution.apply_lifecycle_op(&ShardUpdateOp::InstallRoute {
                address: binding.destination,
                action: TrackRouteAction::Forward {
                    publication_id: publication,
                },
            });
            execution.apply_lifecycle_op(&ShardUpdateOp::InstallShardFabricSource {
                binding,
                encoding: None,
                emission,
            });
            let source = execution.runtime.shard_fabric_source(binding).unwrap();
            execution
                .install_fabric_branch(track, source, receiver, emission)
                .unwrap()
        };
    let original = install(&mut execution, binding, receiver);
    let mut links = LinkControl::new(|| LinkTime::from_nanos(0)).unwrap();
    let (port, mut rx) = links.create_port().unwrap();
    let (_, tx) = links
        .bind_send(Destination::Local {
            port,
            tag: crate::link::delivery_tag(address),
        })
        .unwrap();
    let route = CompiledRoute {
        address,
        tx,
        clock_origin: now,
    };
    let mut plan = TrackPlan::new([], [address], None);
    plan.compiled_remote.push(route.clone());
    producer.apply_plan(&TrackPlanUpdate {
        publication_id: publication,
        plan: Some(plan),
    });
    producer.apply_lifecycle_op(&ShardUpdateOp::BindShardFabricDestination { binding });
    let first = packet(1);
    let value = first.arrival_view(0).unwrap();
    let expected = {
        let mut bytes = [0; pulsebeam_core::fabric::MAX_PACKET_BYTES];
        let len = value.write_local(&mut bytes).unwrap();
        bytes[..len].to_vec()
    };
    let offer = ShardOffer {
        binding,
        arrival: now,
        sequence: 1,
        packet: value.into_owned(),
    };
    let mut full = false;
    for _ in 0..2048 {
        if !route.offer_fabric(offer.clone()) {
            full = true;
            break;
        }
    }
    assert!(full);
    let blocked = producer
        .offer_fabric(source, publisher, now, now, 1, offer.packet.clone())
        .unwrap();
    assert_eq!((blocked.enqueued, blocked.full), (0, 1));
    let mut frames = Vec::new();
    assert!(rx.try_recv_batch(&mut frames, 2048) > 0);
    frames.clear();
    let retry_at = now.checked_add(Duration::from_millis(1)).unwrap();
    let enqueue = |producer: &mut ShardExecution| {
        producer
            .offer_fabric(
                source,
                publisher,
                retry_at,
                retry_at,
                1,
                offer.packet.clone(),
            )
            .unwrap()
    };
    assert_eq!(enqueue(&mut producer).enqueued, 1);
    assert_eq!(enqueue(&mut producer).duplicates, 1);
    assert_eq!(rx.try_recv_batch(&mut frames, 2), 1);
    let mut received = frames.pop().unwrap();
    let ShardFrame::Fabric(ref attempt) = received.packet else {
        unreachable!()
    };
    assert_eq!(attempt.arrival, now);
    let tag = received.tag;
    received.tag = crate::link::delivery_tag(NodeRouteAddress::new(RouteId::new(shard, 3), 1));
    assert!(!bind_received_frame(shard, &mut received));
    received.tag = tag;
    assert!(bind_received_frame(shard, &mut received));
    execution.on_shard_frames([received.packet], now, &Local);
    assert_eq!(
        execution.admit_fabric(original, receiver, now, |_, _, packet, _| {
            let mut bytes = [0; pulsebeam_core::fabric::MAX_PACKET_BYTES];
            let len = packet.write_local(&mut bytes).unwrap();
            assert_eq!(bytes[..len], expected);
            assert_eq!(packet.remaining_hops(), 0);
            Ok::<_, ()>(())
        }),
        Ok(Some(()))
    );

    let large = packet(2);
    assert_eq!(large.wan_plaintext_len(), 1200);
    assert_eq!(
        large.legacy_wire_len(),
        Err(pulsebeam_core::fabric::WireError::Oversize)
    );
    {
        use crate::participant::event::{FabricPublication, ParticipantSink};
        let owner = producer.registry.resolve_mut(publisher).unwrap();
        let mut sink =
            producer
                .pipeline
                .participant_sink(crate::shard::events::ParticipantBinding {
                    participant_id: owner.participant_id,
                    connection_id: owner.connection_id,
                    handle: publisher,
                });
        sink.publish_track_packet(Some(source_track), crate::rtp::RtpPacket::default());
        sink.publish_fabric_packet(FabricPublication {
            reliable: None,
            source,
            arrival: now,
            sequence: 2,
            packet: large.clone(),
        });
    }
    assert_eq!(producer.flush_stream_buffers(retry_at, 8), 2);
    // Move the receiving shard and its live mailbox together, with work
    // outstanding. Execution placement must not change routing identity.
    let (mut execution, mut rx) = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut batch = Vec::new();
                assert_eq!(rx.try_recv_batch(&mut batch, 2), 1);
                let mut moved = batch.pop().unwrap();
                assert!(bind_received_frame(shard, &mut moved));
                execution.on_shard_frames([moved.packet], retry_at, &Local);
                assert_eq!(
                    execution.admit_fabric(original, receiver, retry_at, |_, seq, image, _| {
                        assert_eq!(seq, 2);
                        assert!(image.same_content(&large.arrival_view(7).unwrap()));
                        assert_eq!(image.header(), large.header());
                        assert_eq!(image.remaining_hops(), large.remaining_hops());
                        Ok::<_, ()>(())
                    }),
                    Ok(Some(()))
                );
                (execution, rx)
            })
            .join()
            .unwrap()
    });

    // Removing and re-adding the same live connection cannot grant it the
    // entitlement of mailbox work sent before the removal.
    assert_eq!(
        producer
            .offer_fabric(source, publisher, retry_at, retry_at, 3, packet(3))
            .unwrap()
            .enqueued,
        1
    );
    execution.apply_plan(&TrackPlanUpdate {
        publication_id: publication,
        plan: None,
    });
    execution.apply_plan(&TrackPlanUpdate {
        publication_id: publication,
        plan: Some(TrackPlan::new([receiver_id], [], None)),
    });
    let old_source = execution.runtime.shard_fabric_source(binding).unwrap();
    assert!(
        execution
            .install_fabric_branch(track, old_source, receiver, emission)
            .is_none()
    );
    assert_eq!(rx.try_recv_batch(&mut frames, 1), 1);
    let ShardFrame::Fabric(pending) = frames.pop().unwrap().packet else {
        unreachable!()
    };
    assert_eq!(
        execution.runtime.offer_shard_fabric(retry_at, pending),
        Err(Rejection::Binding)
    );

    // A frame held in the mailbox must not alias a replacement at this slot.
    assert_eq!(
        producer
            .offer_fabric(source, publisher, retry_at, retry_at, 4, packet(4))
            .unwrap()
            .enqueued,
        1
    );
    execution.apply_lifecycle_op(&ShardUpdateOp::RetireRoute { address });
    execution.apply_lifecycle_op(&ShardUpdateOp::RemoveParticipant {
        connection: receiver_id,
        address: receiver_address,
    });
    let (reconnected_id, _, reconnected) = participant(&mut execution, 2, "receiver");
    assert_ne!(reconnected, receiver);
    execution.apply_plan(&TrackPlanUpdate {
        publication_id: publication,
        plan: Some(TrackPlan::new([reconnected_id], [], None)),
    });
    assert!(producer.runtime.retire_fabric_source(source));
    let new_source = producer
        .install_fabric_source(source_track, publisher, emission, None)
        .unwrap();
    let new_binding = ShardBinding {
        destination: NodeRouteAddress::new(RouteId::new(shard, 2), 2),
        incarnation: producer.runtime.fabric_incarnation(new_source).unwrap(),
        ..binding
    };
    let successor = install(&mut execution, new_binding, reconnected);
    assert_eq!(rx.try_recv_batch(&mut frames, 1), 1);
    execution.on_shard_frames([frames.pop().unwrap().packet], retry_at, &Local);
    let mut stale = offer.clone();
    stale.binding.destination = new_binding.destination;
    assert_eq!(
        execution.runtime.offer_shard_fabric(now, stale),
        Err(Rejection::Binding)
    );
    assert_eq!(
        execution.runtime.fabric_handoff_owner(original, receiver),
        None
    );
    assert_eq!(
        execution.admit_fabric(
            successor,
            reconnected,
            now,
            |_, _, _, _| -> Result<(), ()> { panic!("stale image reached the successor") }
        ),
        Ok(None)
    );

    let address = new_binding.destination;
    let (_, tx) = links
        .bind_send(Destination::Local {
            port,
            tag: crate::link::delivery_tag(address),
        })
        .unwrap();
    let mut plan = TrackPlan::new([], [address], None);
    plan.compiled_remote.push(CompiledRoute {
        address,
        tx,
        clock_origin: now,
    });
    producer.apply_plan(&TrackPlanUpdate {
        publication_id: publication,
        plan: Some(plan),
    });
    producer.apply_lifecycle_op(&ShardUpdateOp::BindShardFabricDestination {
        binding: new_binding,
    });
    assert_eq!(
        producer
            .offer_fabric(new_source, publisher, retry_at, retry_at, 4, packet(4))
            .unwrap()
            .enqueued,
        1
    );
    assert_eq!(rx.try_recv_batch(&mut frames, 1), 1);
    let mut received = frames.pop().unwrap();
    assert!(bind_received_frame(shard, &mut received));
    execution.on_shard_frames([received.packet], retry_at, &Local);
    assert_eq!(
        execution.admit_fabric(successor, reconnected, retry_at, |_, sequence, image, _| {
            assert_eq!(sequence, 4);
            assert_eq!(image.payload(), b"opaque");
            Ok::<_, ()>(())
        }),
        Ok(Some(()))
    );
}

#[test]
fn fabric_refresh_budget_is_shared_by_live_branches_of_one_source() {
    use pulsebeam_runtime::time::Duration;
    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let start = Instant::now();
    let mut execution = ShardExecution::new(
        ShardId::new(0),
        4,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, start),
    );
    let (publisher_id, _, publisher) = participant(&mut execution, 1, "publisher");
    let (left_id, _, left) = participant(&mut execution, 2, "left");
    let (right_id, _, right) = participant(&mut execution, 3, "right");
    let publication_id = publisher_id.fixture_publication(TrackKind::Video, "video");
    let track = execution
        .runtime
        .install_track(publication_id, None, Some(publisher), None);
    execution.apply_plan(&TrackPlanUpdate {
        publication_id,
        plan: Some(TrackPlan::new([left_id, right_id], [], None)),
    });
    let emission = Emission::new(Profile::H264, &[ExtensionKind::DependencyDescriptor]).unwrap();
    let source = execution
        .install_fabric_source(track, publisher, emission, std::num::NonZeroU32::new(123))
        .unwrap();
    let target = pulsebeam_core::dd::evidence::DecodeTarget::new(0).unwrap();
    let left_branch = execution
        .install_fabric_filtered_branch(track, source, left, emission, target, start)
        .unwrap();
    assert_eq!(
        execution
            .runtime
            .awaiting_fabric_refresh(source, |key| key == left),
        Some((left_branch, left))
    );
    assert_eq!(
        execution
            .runtime
            .awaiting_fabric_refresh(source, |key| key == right),
        None
    );
    let right_branch = execution
        .install_fabric_branch(track, source, right, emission)
        .unwrap();
    let never = |_, _| panic!("unauthorized or throttled refresh reached publisher");
    assert!(
        !execution
            .runtime
            .request_fabric_refresh(left_branch, right, start, never)
    );
    assert!(
        !execution
            .runtime
            .request_fabric_refresh(left_branch, left, start, |owner, ssrc| {
                assert_eq!((owner, ssrc), (publisher, 123));
                false
            })
    );
    assert!(
        execution
            .runtime
            .request_fabric_refresh(right_branch, right, start, |owner, ssrc| {
                assert_eq!((owner, ssrc), (publisher, 123));
                true
            })
    );
    assert!(!execution.runtime.request_fabric_refresh(
        left_branch,
        left,
        start + Duration::from_millis(499),
        never
    ));
    assert!(execution.runtime.request_fabric_refresh(
        left_branch,
        left,
        start + Duration::from_millis(500),
        |_, _| true
    ));
    execution.apply_plan(&TrackPlanUpdate {
        publication_id,
        plan: None,
    });
    assert!(!execution.runtime.request_fabric_refresh(
        left_branch,
        left,
        start + Duration::from_secs(1),
        never
    ));
}

#[cfg(all(target_os = "linux", not(feature = "sim")))]
#[pulsebeam_runtime::test(crate = "pulsebeam_runtime::__harness")]
async fn wan_publisher_cache_miss_refreshes_only_a_live_exact_route_within_budget() {
    use std::num::NonZeroU32;

    use crate::route::fabric::{
        CompiledPublisherSend, LiveNextHop,
        relay::{MediaRepairLookup, MediaRepairParent},
    };
    use crate::shard_update::PublisherAnchor;
    use pulsebeam_core::fabric::wan::Window;
    use pulsebeam_link::NodeRouteKey;
    use pulsebeam_runtime::time::Duration;

    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let start = Instant::now();
    let shard = ShardId::new(0);
    let mut execution = ShardExecution::new(
        shard,
        4,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, start),
    );
    let (connection, _, publisher) = participant(&mut execution, 1, "publisher");
    let publication = connection.fixture_publication(TrackKind::Video, "video");
    let track = execution
        .runtime
        .install_track(publication, None, Some(publisher), None);
    let emission = Emission::new(Profile::H264, &[]).unwrap();
    let source = execution
        .install_fabric_source(track, publisher, emission, NonZeroU32::new(123))
        .unwrap();
    let incarnation = execution.runtime.fabric_incarnation(source).unwrap();
    let window = Window::new(Profile::H264, 0, 0, NonZeroU32::new(7).unwrap(), 0).unwrap();
    let expires_at = start + Duration::from_secs(4);
    let (_left, _right, _control, peer, tx) =
        crate::link::wan_host::live_send_fixture(window, incarnation, expires_at)
            .await
            .unwrap();
    let now = Instant::now();
    let position = window.start();
    let lifetime = tx.cutoff();
    let route = CompiledPublisherSend {
        publisher: PublisherAnchor {
            publication,
            connection,
            incarnation,
            emission,
        },
        shard,
        next: LiveNextHop {
            peer,
            key: NodeRouteKey::from_bytes(window.ssrc().get().to_be_bytes()),
        },
        expires_at,
        tx: tx,
    };
    let lookup = MediaRepairLookup {
        parent: MediaRepairParent::Publisher(route.clone()),
        position,
    };
    let runtime = &mut execution.runtime;
    let never = |_| panic!("unauthorized or throttled refresh reached publisher");
    assert!(!runtime.request_peer_media_refresh(source, &route, position, now, never));
    assert!(runtime.install_publisher_send(source, route.clone(), lifetime.clone(), now));
    assert!(
        runtime
            .locate_media_repair(lookup, Some(source), now)
            .is_none()
    );
    assert!(!runtime.request_peer_media_refresh(
        source,
        &route,
        position + pulsebeam_core::fabric::wan::WINDOW_SPAN,
        now,
        never
    ));
    let refresh = |ssrc| {
        assert_eq!(ssrc, 123);
        true
    };
    assert!(runtime.request_peer_media_refresh(source, &route, position, now, refresh));
    let retry = now + crate::track::KEYFRAME_DEBOUNCE;
    assert!(!runtime.request_peer_media_refresh(
        source,
        &route,
        position,
        retry - Duration::from_millis(1),
        never
    ));
    assert!(runtime.request_peer_media_refresh(source, &route, position, retry, refresh));
    lifetime.fence();
    assert!(!runtime.request_peer_media_refresh(
        source,
        &route,
        position,
        retry + crate::track::KEYFRAME_DEBOUNCE,
        never
    ));
}

#[test]
fn exact_installed_plan_and_endpoint_incarnations_fence_local_candidate_handoff() {
    let _rng = pulsebeam_runtime::rand::seeded_rng(1);
    let now = Instant::now();
    let mut execution = ShardExecution::new(
        ShardId::new(0),
        4,
        1,
        WallAnchor::new(std::time::SystemTime::UNIX_EPOCH, now),
    );
    let (publisher_id, publisher_address, publisher) = participant(&mut execution, 1, "publisher");
    let (receiver_id, _, receiver) = participant(&mut execution, 2, "receiver");
    let now = Instant::now();
    let publication_id = publisher_id.fixture_publication(TrackKind::Audio, "audio");
    let track = execution
        .runtime
        .install_track(publication_id, None, Some(publisher), None);
    let emission = Emission::new(Profile::Opus, &[ExtensionKind::AudioLevel]).unwrap();
    let core_only = Emission::new(Profile::Opus, &[]).unwrap();
    assert!(
        execution
            .install_fabric_source(track, receiver, emission, None)
            .is_none()
    );
    let source = execution
        .install_fabric_source(track, publisher, emission, None)
        .unwrap();
    assert!(
        execution
            .install_fabric_branch(track, source, receiver, emission)
            .is_none()
    );
    let install_plan = TrackPlanUpdate {
        publication_id,
        plan: Some(TrackPlan::new([receiver_id], [], None)),
    };
    execution.apply_plan(&install_plan);
    assert!(
        execution
            .install_fabric_branch(track, source, receiver, core_only)
            .is_none()
    );
    let original = execution
        .install_fabric_branch(track, source, receiver, emission)
        .unwrap();
    assert_eq!(
        execution.offer_fabric(source, receiver, now, now, 1, packet(1)),
        Err(Rejection::Binding)
    );
    assert_eq!(
        execution
            .offer_fabric(source, publisher, now, now, 1, packet(1))
            .unwrap()
            .enqueued,
        1
    );
    assert_eq!(
        execution.runtime.fabric_handoff_owner(original, receiver),
        Some(crate::shard::router::FabricOwner::Local(publisher))
    );
    assert_eq!(
        execution.runtime.fabric_handoff_owner(original, publisher),
        None
    );
    execution.apply_plan(&TrackPlanUpdate {
        publication_id,
        plan: None,
    });
    assert_eq!(
        execution.runtime.fabric_handoff_owner(original, receiver),
        None
    );
    let must_not_write = |_: &mut crate::participant::Connection,
                          _: u64,
                          _: PacketView<'_>,
                          _: crate::route::fabric::local::OutputEvidence|
     -> Result<(), ()> {
        panic!("an unbound or empty branch must not expose a packet")
    };
    assert_eq!(
        execution.admit_fabric(original, receiver, now, must_not_write),
        Ok(None)
    );
    execution.apply_plan(&install_plan);
    let successor = execution
        .install_fabric_branch(track, source, receiver, emission)
        .unwrap();
    assert_eq!(
        execution.runtime.fabric_handoff_owner(successor, receiver),
        Some(crate::shard::router::FabricOwner::Local(publisher))
    );
    assert_eq!(
        execution.runtime.fabric_handoff_owner(original, receiver),
        None
    );
    assert_eq!(
        execution.admit_fabric(successor, receiver, now, must_not_write),
        Ok(None)
    );
    assert_eq!(
        execution
            .offer_fabric(source, publisher, now, now, 2, packet(2))
            .unwrap()
            .enqueued,
        1
    );
    assert_eq!(
        execution.admit_fabric(original, receiver, now, must_not_write),
        Ok(None)
    );
    assert_eq!(
        execution.admit_fabric(successor, publisher, now, must_not_write),
        Ok(None)
    );
    use crate::rtp::fabric_egress::{
        AdmissionError, CaptureClock, EndpointHints, OutputLeg, SenderLeg, SourceMetadata, Vad,
        timeline,
    };
    let hints = EndpointHints::new(
        Profile::Opus,
        crate::rtp::fabric_ingress::ExtensionForm::OneByte,
        &[],
        None,
    )
    .unwrap();
    let mut leg = SenderLeg::new(
        OutputLeg {
            mid: crate::media::Mid::new("absent").unwrap(),
            primary: Some(1),
            max_wire_bytes: 1200,
            replace_queued: false,
        },
        hints,
        timeline::Timeline::new(500, 1500, 48_000, 64).unwrap(),
    );
    assert_eq!(
        execution.write_fabric_rtp(
            successor,
            receiver,
            now,
            &mut leg,
            SourceMetadata {
                vad: Vad::On,
                capture: CaptureClock::Unmapped,
                same_topology: true,
            }
        ),
        Err(AdmissionError::Rtc(
            pulsebeam_rtc::WriteError::UnknownStream
        )),
    );
    assert_eq!(
        execution.admit_fabric(successor, receiver, now, |_, sequence, packet, _| {
            assert_eq!(sequence, 2);
            assert_eq!(packet.header().sequence, 2);
            Err::<(), _>(pulsebeam_rtc::WriteError::WouldBlock)
        }),
        Err(pulsebeam_rtc::WriteError::WouldBlock)
    );
    assert_eq!(
        execution.admit_fabric(successor, receiver, now, |_, sequence, packet, _| {
            assert_eq!(sequence, 2);
            assert_eq!(packet.payload(), b"opaque");
            assert_eq!(packet.remaining_hops(), 7);
            Ok::<_, ()>(())
        }),
        Ok(Some(()))
    );
    assert_eq!(
        execution.admit_fabric(successor, receiver, now, must_not_write),
        Ok(None)
    );
    execution
        .offer_fabric(source, publisher, now, now, 3, packet(3))
        .unwrap();
    execution.apply_lifecycle_op(&ShardUpdateOp::RemoveParticipant {
        connection: publisher_id,
        address: publisher_address,
    });
    assert_eq!(
        execution.admit_fabric(successor, receiver, now, must_not_write),
        Ok(None)
    );
    assert_eq!(
        execution.offer_fabric(source, publisher, now, now, 4, packet(4)),
        Err(Rejection::Binding)
    );
}
