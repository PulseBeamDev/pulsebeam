use super::{
    EffectiveMediaFormat, ErrorInfo, PublicationAdmissionStatus, PublicationExecution, TopicMode,
    TrackSelector, VideoObservation,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SubscriptionSourceStatus {
    Pending,
    Unavailable,
    Available,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct VideoSubscriptionState {
    pub observation_revision: u64,
    pub intent_revision: u64,
    pub evaluated_revision: u64,
    pub accepted_revision: Option<u64>,
    pub rejected_revision: Option<u64>,
    pub selector: TrackSelector,
    pub configuration: VideoObservation,
    pub local_enabled: bool,
    pub closed: bool,
    pub source_status: SubscriptionSourceStatus,
    pub source_track_id: Option<String>,
    pub source_format: Option<EffectiveMediaFormat>,
    pub admission: PublicationAdmissionStatus,
    pub admission_pending: bool,
    pub error: Option<ErrorInfo>,
    pub execution: PublicationExecution,
    pub execution_error: Option<ErrorInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AudioSubscriptionState {
    pub observation_revision: u64,
    pub intent_revision: u64,
    pub evaluated_revision: u64,
    pub accepted_revision: Option<u64>,
    pub rejected_revision: Option<u64>,
    pub selector: TrackSelector,
    pub local_enabled: bool,
    pub closed: bool,
    pub source_status: SubscriptionSourceStatus,
    pub source_track_id: Option<String>,
    pub source_format: Option<EffectiveMediaFormat>,
    pub admission: PublicationAdmissionStatus,
    pub admission_pending: bool,
    pub error: Option<ErrorInfo>,
    pub execution: PublicationExecution,
    pub execution_error: Option<ErrorInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DataSubscriptionState {
    pub observation_revision: u64,
    pub intent_revision: u64,
    pub evaluated_revision: u64,
    pub accepted_revision: Option<u64>,
    pub rejected_revision: Option<u64>,
    pub selector: Option<TrackSelector>,
    pub delivery: Option<TopicMode>,
    pub local_enabled: bool,
    pub closed: bool,
    pub source_status: SubscriptionSourceStatus,
    pub source_track_id: Option<String>,
    pub admission: PublicationAdmissionStatus,
    pub admission_pending: bool,
    pub error: Option<ErrorInfo>,
    pub execution: PublicationExecution,
    pub execution_error: Option<ErrorInfo>,
    pub terminal_error: Option<ErrorInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DataPublicationState {
    pub observation_revision: u64,
    pub intent_revision: u64,
    pub evaluated_revision: u64,
    pub accepted_revision: Option<u64>,
    pub rejected_revision: Option<u64>,
    pub track_id: String,
    pub delivery: TopicMode,
    pub local_enabled: bool,
    pub closed: bool,
    pub admission: PublicationAdmissionStatus,
    pub admission_pending: bool,
    pub error: Option<ErrorInfo>,
    pub execution: PublicationExecution,
    pub execution_error: Option<ErrorInfo>,
}
