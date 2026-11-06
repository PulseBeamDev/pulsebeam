use crate::entity::{TrackId, TrackKind};
use crate::media::ChannelId;
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
mod session;

#[cfg(test)]
pub(crate) use session::track_descriptor;
pub(crate) use session::{
    ExecutionAction, ExecutionCommand, InputError, NativeSession, OutputError, Preparation, failure,
};

pub(crate) struct ReceiverTarget<'a> {
    pub(crate) source: TrackId,
    pub(crate) key: &'a str,
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
    Waiting(TrackId),
    Ready(Box<Receiver>),
    Policy(ReceiverPolicy),
}

pub(crate) struct Receiver {
    pub(crate) source: Track,
    pub(crate) source_id: String,
    pub(crate) transport: ReceiveTransport,
    pub(crate) policy: ReceiverPolicy,
}

pub(crate) enum ReceiveTransport {
    Sctp { channel: ChannelId, lane: DataLane },
    Rtp(Box<ReceiverRtpConfiguration>),
}

pub(crate) struct ReceiverPolicy {
    pub(crate) enabled: bool,
    pub(crate) video: Option<VideoPolicy>,
}

pub(crate) fn receiver_target(
    spec: &wire::ResourceSpec,
) -> Result<ReceiverTarget<'_>, wire::FailureCode> {
    let Some(wire::resource_spec::Declaration::Subscription(subscription)) = &spec.declaration
    else {
        return Err(wire::FailureCode::Unsupported);
    };
    if !matches!(
        wire::TrackKind::try_from(subscription.kind),
        Ok(wire::TrackKind::Audio | wire::TrackKind::Video | wire::TrackKind::Data)
    ) {
        return Err(wire::FailureCode::Unsupported);
    }
    let source = subscription
        .track_id
        .parse()
        .map_err(|_| wire::FailureCode::InvalidConfiguration)?;
    Ok(ReceiverTarget {
        source,
        key: &subscription.track_id,
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
            Some(wire::track_binding::Transport::Sctp(binding)),
        ) if subscription.kind == wire::TrackKind::Data as i32 => {
            let lane = match wire::DataDelivery::try_from(data.delivery) {
                Ok(wire::DataDelivery::Ordered) => DataLane::Ordered,
                Ok(wire::DataDelivery::Latest) => DataLane::Latest,
                Ok(wire::DataDelivery::Lossy) => DataLane::Lossy,
                _ => return Err(wire::FailureCode::Incompatible),
            };
            let channel = ChannelId::from_value(
                u16::try_from(
                    binding
                        .stream_id
                        .ok_or(wire::FailureCode::InvalidConfiguration)?,
                )
                .map_err(|_| wire::FailureCode::InvalidConfiguration)?,
            );
            Ok(ReceiveTransport::Sctp { channel, lane })
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
        ReceiveTransport::Sctp { .. } => None,
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
    rtp: Option<(MediaKind, bool)>,
) -> Result<ReceiverPolicy, wire::FailureCode> {
    let video = match rtp {
        Some((kind, supports_playout_delay)) => {
            policy::receiver_settings(kind, supports_playout_delay, spec)?
        }
        None => None,
    };
    Ok(ReceiverPolicy {
        enabled: enabled(spec),
        video,
    })
}

pub(crate) struct DataPublication {
    pub(crate) label: String,
    pub(crate) lane: DataLane,
    pub(crate) channel: ChannelId,
    pub(crate) enabled: bool,
}

pub(crate) struct RtpPublication {
    pub(crate) label: String,
    pub(crate) kind: TrackKind,
    pub(crate) binding: CompiledRtpBinding,
    pub(crate) format: Option<wire::CodecFormat>,
    pub(crate) enabled: bool,
}

pub(crate) enum Publication {
    Data(DataPublication),
    Rtp(Box<RtpPublication>),
}

impl Publication {
    pub(crate) fn execution_bytes(&self, spec_bytes: usize) -> usize {
        match self {
            Self::Data(data) => Connection::native_data_execution_bytes(&data.label),
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
            let Some(wire::track_binding::Transport::Sctp(binding)) = data
                .binding
                .as_ref()
                .and_then(|binding| binding.transport.as_ref())
            else {
                return Err(wire::FailureCode::Unsupported);
            };
            let channel = binding
                .stream_id
                .and_then(|id| u16::try_from(id).ok())
                .map(ChannelId::from_value)
                .ok_or(wire::FailureCode::Unsupported)?;
            Publication::Data(DataPublication {
                label: declaration.label.clone(),
                lane,
                channel,
                enabled,
            })
        }
        _ => {
            let capabilities = capabilities.ok_or(wire::FailureCode::Unsupported)?;
            let (label, kind, binding) = policy::publication(spec, capabilities)?;
            let kind = match kind {
                wire::TrackKind::Audio => TrackKind::Audio,
                wire::TrackKind::Video => TrackKind::Video,
                _ => return Err(wire::FailureCode::Unsupported),
            };
            let format = policy::publication_declaration(spec)
                .and_then(|(_, _, parameters)| parameters.codec.as_ref()?.format.clone());
            Publication::Rtp(Box::new(RtpPublication {
                label,
                kind,
                binding,
                format,
                enabled,
            }))
        }
    };
    let bytes = plan.execution_bytes(spec.encoded_len());
    Ok(Some((plan, bytes)))
}
