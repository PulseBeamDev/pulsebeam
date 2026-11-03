use crate::MediaSlot;

pub(crate) const SIGNALING_LABEL: &str = "v1/sys/signaling";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignalingError {
    #[error("signaling message is malformed")]
    Malformed,
    #[error("signaling message has no payload")]
    MissingPayload,
    #[error("signaling state contains an invalid {0}")]
    Invalid(&'static str),
    #[error("media has no negotiated binding for {0:?}")]
    MissingCoordinate(MediaSlot),
    #[error("intent exceeds bounded signaling size")]
    Encode,
}
