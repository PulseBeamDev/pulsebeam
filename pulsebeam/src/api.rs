use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::{delete, get, post},
};
use axum_extra::{TypedHeader, headers::ContentType};
use hyper::header::LOCATION;
use std::sync::Arc;

use crate::controller;
use crate::entity::{ExternalRoomId, ParticipantId, RoomId};

/// Configuration shared across handlers
#[derive(Clone)]
pub struct ApiConfig {
    pub base_path: String,    // e.g. "/api/v1"
    pub default_host: String, // fallback if no Host header, e.g. "localhost:3000"
}

/// Error type for signaling operations
#[derive(thiserror::Error, Debug)]
pub enum SignalingError {
    #[error("join failed: {0}")]
    JoinError(#[from] controller::ControllerError),
    #[error("server is busy, please try again later.")]
    ServiceUnavailable,
    #[error("failed to construct response URL")]
    BadUrl,
    #[error("unknown error: {0}")]
    Unknown(String),
}

impl IntoResponse for SignalingError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            SignalingError::JoinError(controller::ControllerError::OfferInvalid(_))
            | SignalingError::JoinError(controller::ControllerError::OfferRejected(_)) => {
                StatusCode::BAD_REQUEST
            }
            SignalingError::JoinError(controller::ControllerError::ServiceUnavailable)
            | SignalingError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            SignalingError::JoinError(controller::ControllerError::Unknown(_))
            | SignalingError::JoinError(controller::ControllerError::IOError(_))
            | SignalingError::BadUrl
            | SignalingError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

/// Build an absolute URL for Location header
fn build_location(
    headers: &HeaderMap,
    cfg: &ApiConfig,
    path: &str,
) -> Result<String, SignalingError> {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");

    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&cfg.default_host);

    let url = format!("{}://{}{}{}", scheme, host, cfg.base_path, path);

    url.parse::<Uri>().map_err(|_| SignalingError::BadUrl)?;

    Ok(url)
}

#[axum::debug_handler]
async fn join_room(
    Path(room_id): Path<ExternalRoomId>,
    State((mut con, cfg)): State<(controller::ControllerHandle, ApiConfig)>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    headers: HeaderMap,
    raw_offer: String,
) -> Result<impl IntoResponse, SignalingError> {
    let room_id = Arc::new(RoomId::new(room_id));
    let participant_id = Arc::new(ParticipantId::new());

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

    let path = format!(
        "/rooms/{}/participants/{}",
        &room_id.external, &participant_id
    );
    let location_url = build_location(&headers, &cfg, &path)?;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(LOCATION, location_url.parse().unwrap());

    Ok((StatusCode::CREATED, response_headers, answer_sdp))
}

#[axum::debug_handler]
async fn leave_room(
    Path((room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    State((mut con, _cfg)): State<(controller::ControllerHandle, ApiConfig)>,
) -> Result<impl IntoResponse, SignalingError> {
    let room_id = Arc::new(RoomId::new(room_id));
    let participant_id = Arc::new(participant_id);

    let _ = con
        .send_high(controller::ControllerMessage::RemoveParticipant(
            room_id,
            participant_id,
        ))
        .await;

    Ok(StatusCode::NO_CONTENT)
}

async fn healthcheck() -> impl IntoResponse {
    StatusCode::OK
}

/// Router setup
pub fn router(controller: controller::ControllerHandle, cfg: ApiConfig) -> Router {
    let api = Router::new()
        .route("/rooms/{external_room_id}", post(join_room))
        .route(
            "/rooms/{external_room_id}/participants/{participant_id}",
            delete(leave_room),
        );

    Router::new()
        .nest(&cfg.base_path, api)
        .route("/healthz", get(healthcheck))
        .with_state((controller, cfg))
}
