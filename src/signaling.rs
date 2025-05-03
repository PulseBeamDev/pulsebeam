use crate::controller::{ControllerError, ControllerHandle};
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use axum_extra::{TypedHeader, headers::ContentType};

use crate::message::{ParticipantId, RoomId};

#[derive(thiserror::Error, Debug)]
pub enum SignalingError {
    #[error("join failed: {0}")]
    JoinError(#[from] ControllerError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("unknown error: {0}")]
    Unknown(String),
}

impl IntoResponse for SignalingError {
    fn into_response(self) -> Response {
        let status = match self {
            SignalingError::JoinError(ControllerError::OfferInvalid(_)) => StatusCode::BAD_REQUEST,
            SignalingError::JoinError(ControllerError::OfferRejected(_)) => StatusCode::BAD_REQUEST,
            SignalingError::JoinError(ControllerError::ServiceUnavailable) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            SignalingError::JoinError(ControllerError::Unknown(_)) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            SignalingError::JoinError(ControllerError::IOError(_)) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            SignalingError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SignalingError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct ParticipantInfo {
    room: RoomId,
    participant: ParticipantId,
}

#[axum::debug_handler]
async fn spawn_participant(
    Query(info): Query<ParticipantInfo>,
    State(controller): State<ControllerHandle>,
    TypedHeader(content_type): TypedHeader<ContentType>,
    raw_offer: String,
) -> Result<String, SignalingError> {
    // TODO: validate content_type = "application/sdp"

    let answer = controller
        .allocate(info.room, info.participant, raw_offer)
        .await?;

    Ok(answer)
}

pub fn router(controller: ControllerHandle) -> Router {
    Router::new()
        .route("/", post(spawn_participant))
        .with_state(controller)
}
