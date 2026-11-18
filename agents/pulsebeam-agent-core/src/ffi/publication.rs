use alloc::string::String;

use super::{ErrorInfo, MediaKind};

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
pub struct PublicationAdmission {
    pub status: PublicationAdmissionStatus,
    pub connection_generation: Option<u64>,
    pub publication_id: Option<String>,
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
    pub label: String,
    pub closed: bool,
    pub configuration: PublicationConfigurationSnapshot,
    pub admission: PublicationAdmission,
    pub input_attached: bool,
    pub forwarding_requested: bool,
}
