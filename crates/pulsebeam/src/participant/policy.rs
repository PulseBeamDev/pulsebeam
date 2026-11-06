use crate::media::{Mid, Pt, Rid, Ssrc};
use crate::rtp::MediaTime;
use pulsebeam_rtc::{CompiledRtpBinding, MediaKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoPolicy {
    pub height_ceiling: Option<u32>,
    pub minimum_height: u32,
    pub minimum_fps: u32,
    pub priority: u32,
    pub playout_delay: Option<(MediaTime, MediaTime)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecutionScope {
    pub binding: u64,
    pub sequence: u64,
    pub authorization: u64,
}

// Binding references are never reused within a connection, including after retirement.
pub(crate) enum ResourceObservation {
    Activity {
        binding: u64,
        active: bool,
    },
    Receiver {
        binding: u64,
        execution: ReceiverExecution,
    },
    ReceiverBinding {
        binding: u64,
        state: Option<ReceiverBinding>,
    },
    Ended {
        binding: u64,
    },
    Released {
        binding: u64,
    },
    // Local bookkeeping disposal does not retire signaling or prove SID reuse.
    StorageReleased {
        binding: u64,
        sequence: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionFailureKind {
    Invalid,
    Unsupported,
    Conflict,
    Capacity,
    Internal,
    Retired,
}

impl From<pulsebeam_rtc::RtpConfigurationError> for ExecutionFailureKind {
    fn from(error: pulsebeam_rtc::RtpConfigurationError) -> Self {
        use pulsebeam_rtc::RtpConfigurationError;
        match error {
            RtpConfigurationError::Invalid => Self::Invalid,
            RtpConfigurationError::Unsupported => Self::Unsupported,
            RtpConfigurationError::Collision => Self::Conflict,
            RtpConfigurationError::Capacity => Self::Capacity,
            RtpConfigurationError::CapabilityState | RtpConfigurationError::ConnectionState => {
                Self::Internal
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionFailureContext {
    PrepareReceiver,
    ReceiverAudioSlot,
    ReceiverVideoSlot,
    InstallReceiver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecutionFailure {
    pub kind: ExecutionFailureKind,
    pub context: Option<ExecutionFailureContext>,
}

impl From<ExecutionFailureKind> for ExecutionFailure {
    fn from(kind: ExecutionFailureKind) -> Self {
        Self {
            kind,
            context: None,
        }
    }
}

impl ExecutionFailureKind {
    pub(crate) fn at(self, context: ExecutionFailureContext) -> ExecutionFailure {
        ExecutionFailure {
            kind: self,
            context: Some(context),
        }
    }
}

pub(crate) struct PublicationExecution {
    pub track: crate::track::Track,
    pub enabled: bool,
    pub active: bool,
}

// Reuse availability is independent of source readiness and SID quiescence.
#[derive(Clone, Copy)]
pub(crate) struct ReceiverBinding {
    pub available: bool,
    pub rtp: Option<(MediaKind, bool)>,
}

#[derive(Clone, Copy)]
pub(crate) struct ReceiverExecution {
    pub source: crate::entity::TrackId,
    pub enabled: bool,
    pub ready: bool,
    pub failed: bool,
}

pub(crate) struct ReceiverRtpConfiguration {
    pub binding: CompiledRtpBinding,
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub primary: Ssrc,
    pub payload_type: Pt,
    pub kind: MediaKind,
    pub supports_playout_delay: bool,
    pub video: Option<VideoPolicy>,
}
