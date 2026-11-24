use crate::entity::{ParticipantId, PublicationKey, TrackId, TrackKind};
use crate::participant::Connection;
use crate::participant::policy::{
    PublicationExecution, ReceiverExecution, ReceiverRtpConfiguration, VideoPolicy,
};
use crate::track::{DataLane, Track};
use pulsebeam_proto::{prelude::Message as _, state as wire};
use pulsebeam_rtc::{CompiledRtpBinding, MediaKind};

use super::rtp::policy;

#[cfg(test)]
pub(crate) mod fixture;
pub(super) mod handler;
pub(crate) mod handoff;
mod permissions;
mod session;

pub(crate) use super::permissions::ResourcePermissions;

pub(crate) use session::track_descriptor;
pub(crate) use session::{
    DataBranchIds, ExecutionAction, ExecutionCommand, InputError, LookupInterestChange,
    NativeSession, OutputError, Preparation, failure,
};

pub(crate) struct ResolvedSource<'a> {
    pub(crate) track: &'a Track,
    pub(crate) publisher: &'a ParticipantId,
}

impl ResolvedSource<'_> {
    fn is_available(&self) -> bool {
        self.track.meta().track_id.is_some()
            && match self.track {
                Track::Audio(track) => track.format.is_some(),
                Track::Video(track) => track.format.is_some(),
                Track::Data(_) => true,
            }
    }
}

pub(crate) struct ReceiverTarget {
    pub(crate) source: PublicationKey,
    pub(crate) source_room: String,
    pub(crate) wait_for_source: bool,
}

pub(crate) enum PreparedResource {
    Publication(Publication),
    Receiver(ReceiverPreparation),
}

pub(crate) enum ResourceExecution {
    Publication(Box<PublicationExecution>),
    Receiver(ReceiverExecution),
}

impl From<crate::participant::policy::ExecutionFailureKind> for wire::FailureCode {
    fn from(kind: crate::participant::policy::ExecutionFailureKind) -> Self {
        use crate::participant::policy::ExecutionFailureKind;
        match kind {
            ExecutionFailureKind::Invalid => Self::InvalidConfiguration,
            ExecutionFailureKind::Unsupported => Self::Unsupported,
            ExecutionFailureKind::Conflict => Self::Incompatible,
            ExecutionFailureKind::Capacity => Self::CapacityExceeded,
            ExecutionFailureKind::Internal => Self::InternalFailure,
            ExecutionFailureKind::Retired => Self::ResourceGone,
        }
    }
}

fn execution_failure(error: crate::participant::policy::ExecutionFailure) -> wire::Failure {
    use crate::participant::policy::ExecutionFailureContext;
    wire::Failure {
        diagnostic: match error.context {
            None => "",
            Some(ExecutionFailureContext::PrepareReceiver) => {
                "native receiver transport preparation failed"
            }
            Some(ExecutionFailureContext::ReceiverAudioSlot) => {
                "native receiver MID already has an audio output"
            }
            Some(ExecutionFailureContext::ReceiverVideoSlot) => {
                "native receiver MID already has a video output"
            }
            Some(ExecutionFailureContext::InstallReceiver) => {
                "native receiver transport installation failed"
            }
        }
        .to_owned(),
        ..failure(error.kind.into())
    }
}

pub(crate) struct ExecutionDeferral {
    pub(crate) work: Preparation,
    pub(crate) retained_sequence: Option<u64>,
}

pub(crate) struct ExecutionCompletion {
    pub(crate) work: Preparation,
    pub(crate) result: Result<ResourceExecution, crate::participant::policy::ExecutionFailure>,
    pub(crate) released: bool,
    pub(crate) unix_now: u64,
}

enum ExecutionDisposition {
    Accepted,
    Rejected,
    Deferred,
}

pub(crate) enum ReceiverPreparation {
    Waiting(PublicationKey),
    Ready(Box<Receiver>),
    Policy(ReceiverPolicy),
}

pub(crate) struct Receiver {
    pub(crate) source_room: crate::entity::ProjectRoomKey,
    pub(crate) source: Track,
    pub(crate) source_id: String,
    pub(crate) transport: ReceiveTransport,
    pub(crate) policy: ReceiverPolicy,
}

pub(crate) enum ReceiveTransport {
    FabricData(Box<FabricDataReceiver>),
    QuicData(Box<FabricDataReceiver<wire::QuicDataV1Parameters>>),
    Rtp(Box<ReceiverRtpConfiguration>),
}

pub(crate) struct FabricDataReceiver<P = wire::SctpDataV1Parameters> {
    pub(crate) parameters: P,
    pub(crate) endpoint: pulsebeam_proto::data_control::SubscriberBinding,
}

pub(crate) struct ReceiverPolicy {
    pub(crate) enabled: bool,
    pub(crate) video: Option<VideoPolicy>,
}

pub(crate) fn track_kind(kind: i32) -> Option<TrackKind> {
    match wire::TrackKind::try_from(kind).ok()? {
        wire::TrackKind::Audio => Some(TrackKind::Audio),
        wire::TrackKind::Video => Some(TrackKind::Video),
        wire::TrackKind::Data => Some(TrackKind::Data),
        wire::TrackKind::Unspecified => None,
    }
}

pub(crate) fn receiver_target(
    spec: &wire::ResourceSpec,
) -> Result<ReceiverTarget, wire::FailureCode> {
    let Some(wire::resource_spec::Declaration::Subscription(subscription)) = &spec.declaration
    else {
        return Err(wire::FailureCode::Unsupported);
    };
    let kind = track_kind(subscription.kind).ok_or(wire::FailureCode::Unsupported)?;
    let id = subscription
        .publication_id
        .parse()
        .map_err(|_| wire::FailureCode::InvalidConfiguration)?;
    let source = PublicationKey::new(kind, id);
    if kind == TrackKind::Data
        && !matches!(
            subscription
                .binding
                .as_ref()
                .and_then(|binding| binding.transport.as_ref()),
            Some(
                wire::track_binding::Transport::SctpDataV1(_)
                    | wire::track_binding::Transport::QuicDataV1(_)
            )
        )
    {
        return Err(wire::FailureCode::Unsupported);
    }
    Ok(ReceiverTarget {
        source,
        source_room: subscription.source_room.clone(),
        wait_for_source: !matches!(
            subscription
                .binding
                .as_ref()
                .and_then(|binding| binding.transport.as_ref()),
            Some(wire::track_binding::Transport::Rtp(_))
        ),
    })
}

pub(crate) fn receiver(
    spec: &wire::ResourceSpec,
    source: &Track,
    track: &wire::Track,
    capabilities: Option<&wire::RtpCapabilities>,
    data_v1_endpoint: Option<pulsebeam_proto::data_control::SubscriberBinding>,
) -> Result<(ReceiveTransport, ReceiverPolicy), wire::FailureCode> {
    let Some(wire::resource_spec::Declaration::Subscription(subscription)) = &spec.declaration
    else {
        return Err(wire::FailureCode::InvalidConfiguration);
    };
    let transport = match (
        &track.kind,
        subscription
            .binding
            .as_ref()
            .and_then(|binding| binding.transport.as_ref()),
    ) {
        (
            Some(wire::track::Kind::Data(data)),
            Some(
                transport @ (wire::track_binding::Transport::SctpDataV1(_)
                | wire::track_binding::Transport::QuicDataV1(_)),
            ),
        ) if subscription.kind == wire::TrackKind::Data as i32 => {
            let endpoint = data_v1_endpoint
                .filter(|endpoint| Some(endpoint.source) == source.fabric_data_source())
                .ok_or(wire::FailureCode::Incompatible)?;
            match transport {
                wire::track_binding::Transport::SctpDataV1(parameters)
                    if wire::DataV1Binding::Sctp(*parameters).matches_delivery(data.delivery) =>
                {
                    Ok(ReceiveTransport::FabricData(Box::new(FabricDataReceiver {
                        parameters: *parameters,
                        endpoint,
                    })))
                }
                wire::track_binding::Transport::QuicDataV1(parameters)
                    if wire::DataV1Binding::Quic(*parameters).matches_delivery(data.delivery) =>
                {
                    Ok(ReceiveTransport::QuicData(Box::new(FabricDataReceiver {
                        parameters: *parameters,
                        endpoint,
                    })))
                }
                _ => Err(wire::FailureCode::InvalidConfiguration),
            }
        }
        (
            Some(wire::track::Kind::Audio(_) | wire::track::Kind::Video(_)),
            Some(wire::track_binding::Transport::Rtp(_)),
        ) => {
            let capabilities = capabilities.ok_or(wire::FailureCode::Unsupported)?;
            Ok(ReceiveTransport::Rtp(Box::new(policy::receiver(
                spec,
                source,
                capabilities,
            )?)))
        }
        _ => Err(wire::FailureCode::Incompatible),
    }?;
    let video = match &transport {
        ReceiveTransport::Rtp(rtp) => rtp.video.clone(),
        ReceiveTransport::FabricData(_) | ReceiveTransport::QuicData(_) => None,
    };
    Ok((
        transport,
        ReceiverPolicy {
            enabled: enabled(spec),
            video,
        },
    ))
}

fn enabled(spec: &wire::ResourceSpec) -> bool {
    spec.settings
        .as_ref()
        .is_some_and(|settings| settings.enabled == Some(true))
}

pub(crate) fn receiver_policy(
    spec: &wire::ResourceSpec,
    rtp: Option<MediaKind>,
) -> Result<ReceiverPolicy, wire::FailureCode> {
    let video = match rtp {
        Some(kind) => policy::receiver_settings(kind, spec)?,
        None => None,
    };
    Ok(ReceiverPolicy {
        enabled: enabled(spec),
        video,
    })
}

pub(crate) struct DataPublication {
    pub(crate) track_id: TrackId,
    pub(crate) lane: DataLane,
    pub(crate) parameters: wire::DataV1Binding,
    pub(crate) capture_clock: Option<wire::DataV1CaptureClock>,
    pub(crate) enabled: bool,
}

pub(crate) struct RtpPublication {
    pub(crate) track_id: TrackId,
    pub(crate) kind: TrackKind,
    pub(crate) binding: CompiledRtpBinding,
    pub(crate) format: Option<wire::CodecFormat>,
    pub(crate) fabric_dd_video: bool,
    pub(crate) enabled: bool,
}

pub(crate) enum Publication {
    Data(Box<DataPublication>),
    Rtp(Box<RtpPublication>),
}

impl Publication {
    pub(crate) fn execution_bytes(&self, spec_bytes: usize) -> usize {
        match self {
            Self::Data(data) => Connection::native_data_execution_bytes(data.track_id.as_str())
                .saturating_add(
                    data.capture_clock
                        .as_ref()
                        .map_or(0, |clock| clock.id.len().saturating_mul(4)),
                ),
            Self::Rtp(rtp) => Connection::native_rtp_execution_bytes(
                spec_bytes,
                rtp.binding.description().encodings.len(),
            ),
        }
    }
}

pub(crate) fn publication(
    spec: &wire::ResourceSpec,
    capabilities: Option<&wire::RtpCapabilities>,
) -> Result<Option<(Publication, usize)>, wire::FailureCode> {
    let Some(wire::resource_spec::Declaration::Publication(declaration)) = &spec.declaration else {
        return Ok(None);
    };
    let enabled = enabled(spec);
    let plan = match &declaration.kind {
        Some(wire::publication_spec::Kind::Data(data)) => {
            let lane = match wire::DataDelivery::try_from(data.delivery) {
                Ok(wire::DataDelivery::Latest) => DataLane::Latest,
                Ok(wire::DataDelivery::Lossy) => DataLane::Lossy,
                Ok(wire::DataDelivery::Ordered) => DataLane::Ordered,
                _ => return Err(wire::FailureCode::Unsupported),
            };
            let parameters = data
                .binding
                .as_ref()
                .and_then(wire::DataV1Binding::from_track)
                .ok_or(wire::FailureCode::Unsupported)?;
            if !parameters.matches_delivery(data.delivery) {
                return Err(wire::FailureCode::InvalidConfiguration);
            }
            Publication::Data(Box::new(DataPublication {
                track_id: TrackId::new(&declaration.track_id)
                    .map_err(|_| wire::FailureCode::InvalidConfiguration)?,
                lane,
                parameters,
                capture_clock: data.capture_clock.clone(),
                enabled,
            }))
        }
        _ => {
            let capabilities = capabilities.ok_or(wire::FailureCode::Unsupported)?;
            let (track_id, kind, binding) = policy::publication(spec, capabilities)?;
            let kind = match kind {
                wire::TrackKind::Audio => TrackKind::Audio,
                wire::TrackKind::Video => TrackKind::Video,
                _ => return Err(wire::FailureCode::Unsupported),
            };
            let (_, _, parameters) = policy::publication_declaration(spec)
                .ok_or(wire::FailureCode::InvalidConfiguration)?;
            let format = parameters
                .codec
                .as_ref()
                .and_then(|codec| codec.format.clone());
            let fabric_dd_video = kind == TrackKind::Video
                && binding.description().codec.parameters.name == "h264"
                && (parameters.encodings.len() > 1
                    || parameters.encodings.iter().any(|encoding| {
                        encoding.scalability_mode.as_deref()
                            == Some(wire::RtpScalabilityMode::L1t3.as_str())
                    }));
            Publication::Rtp(Box::new(RtpPublication {
                track_id,
                kind,
                binding,
                format,
                fabric_dd_video,
                enabled,
            }))
        }
    };
    let bytes = plan.execution_bytes(spec.encoded_len());
    Ok(Some((plan, bytes)))
}
