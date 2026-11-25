use alloc::{string::String, vec::Vec};

use super::{ErrorInfo, MediaKind};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum VideoCodec {
    #[default]
    H264,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct VideoPublishOptions {
    #[uniffi(default = true)]
    pub enabled: bool,
    /// Omission requires H264. The codec cannot change for this publication.
    #[uniffi(default = None)]
    pub codec: Option<VideoCodec>,
}

impl Default for VideoPublishOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            codec: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PublicationAdmissionStatus {
    Pending,
    Admitted,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PublicationDemand {
    Unknown,
    Idle,
    Active,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PublicationExecution {
    Unavailable,
    Disabled,
    Installed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PublicationConfigurationSnapshot {
    pub desired_revision: u64,
    pub evaluated_revision: u64,
    pub accepted_revision: u64,
    pub enabled: bool,
    pub accepted_enabled: bool,
    pub rejected_revision: Option<u64>,
    pub error: Option<ErrorInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MediaFormatParameter {
    pub name: String,
    pub value: String,
}

/// The admitted media format, independent of transport-local PT, RTX and SSRC.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct EffectiveMediaFormat {
    pub mime_type: String,
    pub clock_rate: u32,
    pub channels: Option<u32>,
    pub parameters: Vec<MediaFormatParameter>,
}

impl EffectiveMediaFormat {
    pub(crate) fn from_track(track: &pulsebeam_proto::state::Track) -> Option<Self> {
        use pulsebeam_proto::state::track::Kind;
        let format = match track.kind.as_ref()? {
            Kind::Audio(audio) => audio.format.as_ref(),
            Kind::Video(video) => video.format.as_ref(),
            Kind::Data(_) => None,
        }?;
        Some(Self {
            mime_type: format.mime_type.clone(),
            clock_rate: format.clock_rate,
            channels: format.channels,
            parameters: format
                .parameters
                .iter()
                .map(|parameter| MediaFormatParameter {
                    name: parameter.name.clone(),
                    value: parameter.value.clone(),
                })
                .collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PublicationAdmission {
    pub status: PublicationAdmissionStatus,
    pub connection_generation: Option<u64>,
    pub publication_id: Option<String>,
    pub effective_format: Option<EffectiveMediaFormat>,
    pub evaluation_pending: bool,
    pub error: Option<ErrorInfo>,
    pub execution: PublicationExecution,
    pub execution_error: Option<ErrorInfo>,
    pub demand: PublicationDemand,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PublicationState {
    pub observation_revision: u64,
    pub kind: MediaKind,
    pub track_id: String,
    pub closed: bool,
    pub configuration: PublicationConfigurationSnapshot,
    pub admission: PublicationAdmission,
    pub input_attached: bool,
    pub forwarding_requested: bool,
}
