use std::sync::Arc;

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use axum_extra::{TypedHeader, headers::ContentType};
use str0m::{change::SdpOffer, error::SdpError};
use tokio::sync::oneshot;

use crate::{
    manager::ManagerHandle,
    message::{GroupId, JoinError, JoinRequest, PeerId},
};

#[derive(thiserror::Error, Debug)]
pub enum SignalingError {
    #[error("join failed: {0}")]
    JoinError(#[from] JoinError),

    #[error("sdp offer is invalid: {0}")]
    OfferInvalid(#[from] SdpError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("unknown error: {0}")]
    Unknown(String),
}

impl IntoResponse for SignalingError {
    fn into_response(self) -> Response {
        let status = match self {
            SignalingError::JoinError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SignalingError::OfferInvalid(_) => StatusCode::BAD_REQUEST,
            SignalingError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SignalingError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct PeerInfo {
    group_id: GroupId,
    peer_id: PeerId,
}

#[axum::debug_handler]
async fn spawn_peer(
    Query(peer): Query<PeerInfo>,
    State(handle): State<ManagerHandle>,
    TypedHeader(content_type): TypedHeader<ContentType>,
    raw_offer: String,
) -> Result<String, SignalingError> {
    // TODO: validate content_type = "application/sdp"

    let offer = SdpOffer::from_sdp_string(&raw_offer)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .join(JoinRequest {
            group_id: Arc::new(peer.group_id),
            peer_id: Arc::new(peer.peer_id),
            offer,
            reply: reply_tx,
        })
        .map_err(|err| SignalingError::Unknown(err.to_string()))?;
    let answer = reply_rx
        .await
        .map_err(|_| SignalingError::ServiceUnavailable)??;

    Ok(answer.to_sdp_string())
}

pub fn router(handle: ManagerHandle) -> Router {
    Router::new().route("/", get(spawn_peer)).with_state(handle)
}
