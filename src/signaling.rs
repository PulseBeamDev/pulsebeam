use crate::entity::{ParticipantId, RoomId};
use crate::rng::Rng;
use crate::{
    controller::{ControllerError, ControllerHandle},
    entity::{ExternalParticipantId, ExternalRoomId},
};
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, post},
};
use axum_extra::{TypedHeader, headers::ContentType};
use hyper::HeaderMap;
use hyper::header::LOCATION;

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
    room: ExternalRoomId,
    participant: ExternalParticipantId,
}

#[axum::debug_handler]
async fn spawn_participant(
    Query(info): Query<ParticipantInfo>,
    State((mut rng, controller)): State<(Rng, ControllerHandle)>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    raw_offer: String,
) -> Result<impl IntoResponse, SignalingError> {
    // TODO: validate content_type = "application/sdp"

    let room_id = RoomId::new(info.room);
    let participant_id = ParticipantId::new(&mut rng, info.participant);

    // TODO: better unique ID to handle session.
    let location_url = format!("/rooms/{}/participants/{}", &room_id, &participant_id,);
    let answer = controller
        .allocate(room_id, participant_id, raw_offer)
        .await?;

    let mut headers = HeaderMap::new();
    headers.insert(LOCATION, location_url.parse().unwrap());
    let resp = (StatusCode::CREATED, headers, answer);
    Ok(resp)
}

#[axum::debug_handler]
async fn delete_participant(
    State(_state): State<(Rng, ControllerHandle)>,
) -> Result<impl IntoResponse, SignalingError> {
    // TODO: delete participant from the room
    Ok(StatusCode::OK)
}

pub fn router(rng: Rng, controller: ControllerHandle) -> Router {
    Router::new()
        .route("/", post(spawn_participant))
        .route("/", delete(delete_participant))
        .with_state((rng, controller))
}
