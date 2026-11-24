use std::collections::{BTreeMap, VecDeque};
use std::num::{NonZeroU32, NonZeroU64};

use pulsebeam_agent_quic::{
    data_bindings::{Bindings, DataBinding, InstallError, PayloadBinding},
    records::{Datagram, DatagramKind, PictureLossIndication},
    transport::{DatagramAdmission, listener::Association},
};
use pulsebeam_proto::{
    data_endpoint::Delivery,
    state::{self as wire, validation::QuicRtpBinding},
};
use pulsebeam_runtime::time::Instant;

use super::{ExecutionAction, ExecutionCompletion, ExecutionDeferral, Fact, Installed, ShardEvent};
use crate::{
    control::native::{
        DataPublication, Preparation, PreparedResource, Publication, QuicRtpPublication,
        ResourceExecution,
    },
    entity::{PublicationId, PublicationKey},
    participant::{
        data_fabric::QuicInstallation,
        policy::{
            ExecutionFailure, ExecutionFailureKind, PublicationExecution, ResourceObservation,
        },
    },
    shard::{core::ShardCore, router::FabricSourceHandle},
    track::{Track, TrackMeta},
};

const MAX_PUBLISHERS: usize = 64;

pub(super) struct Publishers {
    bindings: Bindings<PayloadBinding, (FabricSourceHandle, u64)>,
    owned: BTreeMap<u64, Publisher>,
    pending_refresh: Option<(FabricSourceHandle, u32, Instant)>,
}

struct Publisher {
    source: FabricSourceHandle,
    binding: PayloadBinding,
    rtp: Option<(
        wire::QuicRtpParameters,
        crate::rtp::fabric_ingress::QuicRtpIngress,
    )>,
    retiring: bool,
    execution: PublicationExecution,
    sequence: u64,
}

impl Publisher {
    fn reliable_live(&self) -> bool {
        !self.retiring
            && self.execution.enabled
            && matches!(self.binding, PayloadBinding::Data(data) if data.format.delivery() == Delivery::Reliable)
    }
}

impl Default for Publishers {
    fn default() -> Self {
        Self {
            bindings: Bindings::new(MAX_PUBLISHERS),
            owned: BTreeMap::new(),
            pending_refresh: None,
        }
    }
}

impl Installed {
    pub(super) fn execute(
        &mut self,
        association: NonZeroU64,
        action: ExecutionAction,
        core: &mut ShardCore,
        events: &mut VecDeque<ShardEvent>,
    ) -> bool {
        let fact = match action {
            ExecutionAction::Prepare { work, resource } => {
                let current = core.current_quic_grant(self.connection, self.transport, association);
                if let Some((_, unix_now)) =
                    current.filter(|(grant, _)| grant.revision == work.scope().authorization)
                {
                    let result = match resource {
                        PreparedResource::Publication(Publication::Data(plan)) => {
                            self.prepare_publisher(association, work, *plan, core, events)
                        }
                        PreparedResource::Publication(Publication::QuicRtp(plan)) => {
                            self.prepare_rtp_publisher(association, work, *plan, core, events)
                        }
                        PreparedResource::Receiver(plan) => {
                            self.prepare_receiver(work, plan, core, events)
                        }
                        _ => Err(ExecutionFailureKind::Unsupported.into()),
                    };
                    Fact::Completion(ExecutionCompletion {
                        work,
                        released: result.is_err()
                            && !self.publishers.owned.contains_key(&work.reference)
                            && self.receiver_sequence(work.reference).is_none(),
                        result,
                        unix_now,
                    })
                } else {
                    Fact::Deferral(ExecutionDeferral {
                        work,
                        retained_sequence: self
                            .publishers
                            .owned
                            .get(&work.reference)
                            .map(|value| value.sequence)
                            .or_else(|| self.receiver_sequence(work.reference)),
                    })
                }
            }
            ExecutionAction::Retire(scope) => {
                if let Some(released) = self.retire_receiver(scope, core, events) {
                    return released;
                }
                if let Some(previous) = self.publishers.owned.get_mut(&scope.binding) {
                    if previous.sequence > scope.sequence || previous.retiring {
                        return true;
                    }
                    previous.retiring = true;
                    let meta = previous.execution.track.meta().clone();
                    let _ = self.publishers.bindings.retire(previous.binding);
                    if previous.rtp.is_some() {
                        core.set_quic_media_enabled(previous.source, association, false);
                    }
                    events.push_back(ShardEvent::OwnedTrackUnpublished {
                        meta,
                        reference: scope.binding,
                    });
                    return true;
                }
                Fact::Resource(ResourceObservation::Released {
                    binding: scope.binding,
                })
            }
            ExecutionAction::FailReceiver(scope) => Fact::Resource(ResourceObservation::Released {
                binding: scope.binding,
            }),
        };
        self.handoff.fact(fact)
    }

    fn prepare_publisher(
        &mut self,
        association: NonZeroU64,
        work: Preparation,
        plan: DataPublication,
        core: &mut ShardCore,
        events: &mut VecDeque<ShardEvent>,
    ) -> Result<ResourceExecution, ExecutionFailure> {
        let room = self
            .scope
            .joined_room()
            .ok_or(ExecutionFailureKind::Invalid)?;
        let pulsebeam_proto::state::DataV1Binding::Quic(parameters) = plan.parameters else {
            return Err(ExecutionFailureKind::Unsupported.into());
        };
        let binding =
            DataBinding::from_parameters(parameters).ok_or(ExecutionFailureKind::Invalid)?;
        if let Some(previous) = self.publishers.owned.get_mut(&work.reference) {
            if previous.retiring
                || previous.binding != PayloadBinding::Data(binding)
                || previous.sequence >= work.scope().sequence
            {
                return Err(ExecutionFailureKind::Conflict.into());
            }
            previous.execution.enabled = plan.enabled;
            previous.sequence = work.scope().sequence;
            return Ok(ResourceExecution::Publication(Box::new(
                previous.execution.clone(),
            )));
        }
        if self.publishers.owned.len() == MAX_PUBLISHERS {
            return Err(ExecutionFailureKind::Capacity.into());
        }
        let publisher = core
            .quic_owner_handle(self.transport)
            .ok_or(ExecutionFailureKind::Retired)?;
        let id = PublicationKey::Data(PublicationId::mint());
        let installation = QuicInstallation {
            association,
            binding,
        };
        let clock = plan
            .capture_clock
            .as_ref()
            .map(|clock| pulsebeam_proto::data_record::Clock {
                id: &clock.id,
                epoch: clock.epoch,
            });
        let (source, incarnation) = core
            .install_quic_data_source(publisher, id, installation, clock)
            .ok_or(ExecutionFailureKind::Capacity)?;
        if let Err(error) = self
            .publishers
            .bindings
            .install(PayloadBinding::Data(binding), (source, work.reference))
        {
            core.retire_quic_publication(id);
            return Err(match error {
                InstallError::Capacity => ExecutionFailureKind::Capacity,
                InstallError::Reused => ExecutionFailureKind::Conflict,
            }
            .into());
        }
        let meta = TrackMeta {
            room,
            id,
            origin: self.participant,
            connection_id: self.connection,
            track_id: Some(plan.track_id),
        };
        let track =
            Track::native_data_publication(meta, plan.lane, Some(incarnation), plan.capture_clock);
        events.push_back(ShardEvent::TrackPublished(track.clone()));
        let execution = PublicationExecution {
            track,
            data_v1_source: Some(incarnation),
            enabled: plan.enabled,
            active: false,
        };
        self.publishers.owned.insert(
            work.reference,
            Publisher {
                source,
                binding: PayloadBinding::Data(binding),
                rtp: None,
                retiring: false,
                sequence: work.scope().sequence,
                execution: execution.clone(),
            },
        );
        Ok(ResourceExecution::Publication(Box::new(execution)))
    }

    fn prepare_rtp_publisher(
        &mut self,
        association: NonZeroU64,
        work: Preparation,
        plan: QuicRtpPublication,
        core: &mut ShardCore,
        events: &mut VecDeque<ShardEvent>,
    ) -> Result<ResourceExecution, ExecutionFailure> {
        use crate::{
            entity::TrackKind,
            route::fabric::{RtpSourceEncoding, local::Emission},
            track::AudioTrack,
        };
        let kind = match plan.kind {
            TrackKind::Audio => wire::TrackKind::Audio,
            TrackKind::Video => wire::TrackKind::Video,
            _ => return Err(ExecutionFailureKind::Unsupported.into()),
        };
        let room = self
            .scope
            .joined_room()
            .ok_or(ExecutionFailureKind::Invalid)?;
        let prepared = QuicRtpBinding::compile(&plan.parameters, kind)
            .map_err(|_| ExecutionFailureKind::Invalid)?;
        let binding = PayloadBinding::from(prepared);
        let ingress = crate::rtp::fabric_ingress::QuicRtpIngress::new(prepared)
            .map_err(|_| ExecutionFailureKind::Invalid)?;
        let sequence = work.scope().sequence;
        if let Some(previous) = self.publishers.owned.get_mut(&work.reference) {
            if previous.retiring
                || previous.binding != binding
                || previous.rtp.as_ref().map(|(parameters, _)| parameters) != Some(&plan.parameters)
                || previous.sequence >= sequence
            {
                return Err(ExecutionFailureKind::Conflict.into());
            }
            if !core.set_quic_media_enabled(previous.source, association, plan.enabled) {
                return Err(ExecutionFailureKind::Retired.into());
            }
            previous.execution.enabled = plan.enabled;
            previous.sequence = sequence;
            return Ok(ResourceExecution::Publication(Box::new(
                previous.execution.clone(),
            )));
        }
        if self.publishers.owned.len() >= MAX_PUBLISHERS {
            return Err(ExecutionFailureKind::Capacity.into());
        }
        let publisher = core
            .quic_owner_handle(self.transport)
            .ok_or(ExecutionFailureKind::Retired)?;
        let id = PublicationKey::new(plan.kind, PublicationId::mint());
        let encoding = RtpSourceEncoding {
            primary: prepared.ssrc().get().into(),
            rid: None,
            payload_type: prepared.payload_type().into(),
        };
        let emission =
            Emission::new(ingress.profile(), &[]).ok_or(ExecutionFailureKind::Invalid)?;
        let (source, incarnation) = core
            .install_quic_rtp_source(publisher, id, association, plan.enabled, encoding, emission)
            .ok_or(ExecutionFailureKind::Capacity)?;
        if let Err(error) = self
            .publishers
            .bindings
            .install(binding, (source, work.reference))
        {
            core.retire_quic_publication(id);
            return Err(match error {
                InstallError::Capacity => ExecutionFailureKind::Capacity,
                InstallError::Reused => ExecutionFailureKind::Conflict,
            }
            .into());
        }
        let meta = TrackMeta {
            room,
            id,
            origin: self.participant,
            connection_id: self.connection,
            track_id: Some(plan.track_id),
        };
        let format = Some(prepared.format().clone());
        let track = if plan.kind == TrackKind::Audio {
            Track::Audio(AudioTrack {
                meta: meta.clone(),
                format,
                reverse: None,
            })
        } else {
            let mut track = Track::video(
                meta.clone(),
                vec![crate::track::TrackLayer {
                    meta: meta.clone(),
                    rid: None,
                    encoding: crate::track::SourceEncoding::Rid(None),
                    quality: crate::track::LayerQuality::Low,
                }],
                None,
            );
            if let Track::Video(video) = &mut track {
                video.format = format;
            }
            track
        };
        events.push_back(ShardEvent::TrackPublished(track.clone()));
        events.push_back(ShardEvent::LocalFabricRtpSourceReady {
            meta,
            encoding,
            incarnation,
            emission,
        });
        let execution = PublicationExecution {
            track,
            data_v1_source: None,
            enabled: plan.enabled,
            active: false,
        };
        self.publishers.owned.insert(
            work.reference,
            Publisher {
                source,
                binding,
                rtp: Some((plan.parameters, ingress)),
                retiring: false,
                execution: execution.clone(),
                sequence,
            },
        );
        Ok(ResourceExecution::Publication(Box::new(execution)))
    }

    pub(super) fn poll_publisher_refresh(
        &mut self,
        association: NonZeroU64,
        peer: &mut Association,
        core: &mut ShardCore,
        now: Instant,
    ) -> bool {
        if self.handoff.execution_stopped()
            || core
                .current_quic_grant(self.connection, self.transport, association)
                .is_none()
        {
            return false;
        }
        let Some(owner) = core.quic_owner_handle(self.transport) else {
            return false;
        };
        let Some((source, ssrc, at)) = self
            .publishers
            .pending_refresh
            .take()
            .or_else(|| core.pop_quic_publisher_refresh(owner))
        else {
            return false;
        };
        if now.saturating_duration_since(at) >= crate::track::KEYFRAME_DEBOUNCE {
            return true;
        }
        let Some(binding) = self.publishers.owned.values().find_map(|publisher| {
            let PayloadBinding::Rtp { id, .. } = publisher.binding else {
                return None;
            };
            let (parameters, ingress) = publisher.rtp.as_ref()?;
            (publisher.source == source
                && !publisher.retiring
                && publisher.execution.enabled
                && ingress.profile() == pulsebeam_core::fabric::Profile::H264
                && parameters.primary_ssrc == ssrc)
                .then_some(id)
        }) else {
            return true;
        };
        let Some(media_ssrc) = NonZeroU32::new(ssrc) else {
            return true;
        };
        let pli = PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc,
        }
        .encode();
        if let Err(DatagramAdmission::ControlTurn | DatagramAdmission::Full) =
            peer.scheduler.offer_bound(
                &mut peer.connection,
                Datagram {
                    kind: DatagramKind::Rtcp,
                    binding,
                    payload: &pli,
                },
            )
        {
            self.publishers.pending_refresh = Some((source, ssrc, at));
        }
        true
    }

    pub(super) fn ingest_payload(
        &self,
        association: NonZeroU64,
        packet: Datagram<'_>,
        core: &mut ShardCore,
        now: Instant,
    ) -> Option<(u64, u64)> {
        if self.handoff.execution_stopped()
            || core
                .current_quic_grant(self.connection, self.transport, association)
                .is_none()
        {
            return None;
        }
        if packet.kind == DatagramKind::Rtcp {
            let receiver = core.quic_owner_handle(self.transport)?;
            self.ingest_receiver_pli(receiver, packet, core);
            return None;
        }
        let (binding, (source, reference)) = self.publishers.bindings.resolve(packet.binding)?;
        let installed = self.publishers.owned.get(reference)?;
        if !installed.execution.enabled || installed.retiring {
            return None;
        }
        let publisher = core.quic_owner_handle(self.transport)?;
        let binding = match (packet.kind, binding) {
            (DatagramKind::Data, PayloadBinding::Data(binding)) => binding,
            (DatagramKind::Rtp, PayloadBinding::Rtp { .. }) => {
                let (_, ingress) = installed.rtp.as_ref()?;
                core.ingest_quic_rtp(*source, publisher, *ingress, packet.payload, now);
                return None;
            }
            _ => return None,
        };
        core.ingest_quic_data(
            *source,
            publisher,
            QuicInstallation {
                association,
                binding,
            },
            packet.payload,
            now,
        )
        .map(|unit_seq| (*reference, unit_seq))
    }

    pub(super) fn publisher_admission(&self, reference: u64, unit_seq: u64) -> Option<Vec<u8>> {
        use pulsebeam_proto::{data_control::PublisherBinding, state::codec};
        let publisher = self.publishers.owned.get(&reference)?;
        if publisher.retiring || !publisher.execution.enabled {
            return None;
        }
        let source = publisher.execution.data_v1_source?;
        let frame = crate::participant::data_fabric::publisher_admission_observation(
            reference,
            PublisherBinding { source },
            unit_seq,
        )
        .ok()?;
        codec::encode_data_control(&frame).ok()
    }

    pub(super) fn publisher_control(
        &self,
        source: FabricSourceHandle,
        record: pulsebeam_proto::data_control::publisher_observation::Record,
    ) -> Option<Vec<u8>> {
        use pulsebeam_proto::{data_control::PublisherBinding, state::codec};
        let (&owned, publisher) = self
            .publishers
            .owned
            .iter()
            .find(|(_, publisher)| publisher.source == source && publisher.reliable_live())?;
        let endpoint = PublisherBinding {
            source: publisher.execution.data_v1_source?,
        };
        let frame =
            crate::participant::data_fabric::publisher_observation(owned, endpoint, record).ok()?;
        codec::encode_data_control(&frame).ok()
    }

    pub(super) fn receive_publisher_control(
        &self,
        association: NonZeroU64,
        frame: pulsebeam_proto::state::OwnedDataControl,
        core: &mut ShardCore,
        now: Instant,
    ) {
        use pulsebeam_proto::data_control::PublisherBinding;
        if self.handoff.execution_stopped()
            || core
                .current_quic_grant(self.connection, self.transport, association)
                .is_none()
        {
            return;
        }
        let Some(publisher) = self
            .publishers
            .owned
            .get(&frame.owned)
            .filter(|publisher| publisher.reliable_live())
        else {
            return;
        };
        let Some(incarnation) = publisher.execution.data_v1_source else {
            return;
        };
        let Some(demand) = crate::participant::data_fabric::decode_original_unavailable(
            PublisherBinding {
                source: incarnation,
            },
            &frame.record,
        ) else {
            return;
        };
        if let Some(owner) = core.quic_owner_handle(self.transport) {
            core.settle_quic_original_unavailable(publisher.source, owner, demand, now);
        }
    }

    pub(super) fn publication_cutoff_result(&mut self, reference: u64, success: bool) {
        if !self
            .publishers
            .owned
            .get(&reference)
            .is_some_and(|publisher| publisher.retiring)
        {
            return;
        }
        if success {
            self.publishers.owned.remove(&reference);
            self.failed |= !self
                .handoff
                .fact(Fact::Resource(ResourceObservation::Released {
                    binding: reference,
                }));
        } else {
            self.failed = true;
        }
    }

    pub(super) fn retire_publishers(&self, core: &mut ShardCore) {
        for publisher in self.publishers.owned.values() {
            core.retire_quic_publication(publisher.execution.track.id());
        }
    }
}
