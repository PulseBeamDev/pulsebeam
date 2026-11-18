use super::ErrorInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PipeStatus {
    Pending,
    Available,
    Unavailable,
    Closed,
}

/// Input availability is independent of publication admission and receiver demand.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PipeState {
    pub observation_revision: u64,
    pub status: PipeStatus,
    pub error: Option<ErrorInfo>,
}
