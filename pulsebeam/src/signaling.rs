use std::sync::Arc;

use crate::controller;
use crate::entity::{ExternalRoomId, ParticipantId, RoomId};
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
};
use axum_extra::TypedHeader;
use axum_extra::headers::ContentType;
use hyper::header::LOCATION;

/// Error type for signaling operations
#[derive(thiserror::Error, Debug)]
pub enum SignalingError {
    #[error("join failed: {0}")]
    JoinError(#[from] controller::ControllerError),

    #[error("server is busy, please try again later.")]
    ServiceUnavailable,

    #[error("unknown error: {0}")]
    Unknown(String),
}

impl IntoResponse for SignalingError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            SignalingError::JoinError(controller::ControllerError::OfferInvalid(_)) => {
                StatusCode::BAD_REQUEST
            }
            SignalingError::JoinError(controller::ControllerError::OfferRejected(_)) => {
                StatusCode::BAD_REQUEST
            }
            SignalingError::JoinError(controller::ControllerError::ServiceUnavailable) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            SignalingError::JoinError(controller::ControllerError::Unknown(_)) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            SignalingError::JoinError(controller::ControllerError::IOError(_)) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            SignalingError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SignalingError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, self.to_string()).into_response()
    }
}

#[axum::debug_handler]
async fn join_room(
    Path(room_id): Path<ExternalRoomId>,
    State(mut con): State<controller::ControllerHandle>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    raw_offer: String,
) -> Result<impl IntoResponse, SignalingError> {
    let room_id = RoomId::new(room_id);
    let room_id = Arc::new(room_id);
    let participant_id = ParticipantId::new();
    let participant_id = Arc::new(participant_id);

    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    con.send_high(controller::ControllerMessage::Allocate(
        room_id.clone(),
        participant_id.clone(),
        raw_offer,
        answer_tx,
    ))
    .await
    .map_err(|_| controller::ControllerError::ServiceUnavailable)?;
    let answer_sdp = answer_rx
        .await
        .map_err(|_| controller::ControllerError::ServiceUnavailable)??;

    // TODO: remove hardcoded URI
    let location_url = format!(
        "http://localhost:3000/api/v1/rooms/{}/participants/{}",
        &room_id.external, &participant_id
    );
    let mut response_headers = HeaderMap::new();
    response_headers.insert(LOCATION, location_url.parse().unwrap());
    Ok((StatusCode::CREATED, response_headers, answer_sdp))
}

#[axum::debug_handler]
async fn leave_room(
    Path((room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    State(mut con): State<controller::ControllerHandle>,
) -> Result<impl IntoResponse, SignalingError> {
    let room_id = RoomId::new(room_id);
    let room_id = Arc::new(room_id);
    let participant_id = Arc::new(participant_id);

    // If controller has exited, there's no dangling participants and rooms
    let _ = con
        .send_high(controller::ControllerMessage::RemoveParticipant(
            room_id,
            participant_id,
        ))
        .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Healthcheck endpoint for Kubernetes
/// GET /healthz
async fn healthcheck() -> impl IntoResponse {
    StatusCode::OK
}

/// Router setup
pub fn router(controller: controller::ControllerHandle) -> Router {
    Router::new()
        .route("/api/v1/rooms/{external_room_id}", post(join_room))
        .route(
            "/api/v1/rooms/{external_room_id}/participants/{participant_id}",
            delete(leave_room),
        )
        .route("/healthz", get(healthcheck))
        .with_state(controller)
}
