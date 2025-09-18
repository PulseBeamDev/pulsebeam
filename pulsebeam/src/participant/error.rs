use prost::DecodeError;
use str0m::{RtcError, error::SdpError};

#[derive(thiserror::Error, Debug)]
pub enum ParticipantError {
    #[error("invalid sdp format: {0}")]
    InvalidSdpFormat(#[from] SdpError),

    #[error(transparent)]
    OfferRejected(#[from] RtcError),

    #[error("invalid rpc format: {0}")]
    InvalidRPCFormat(#[from] DecodeError),
}
