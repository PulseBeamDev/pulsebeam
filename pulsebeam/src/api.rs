use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::{delete, post},
};
use axum_extra::{TypedHeader, headers::ContentType};
use hyper::header::LOCATION;
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::controller;
use crate::entity::{ExternalRoomId, ParticipantId, RoomId};

pub enum HeaderExt {
    ParticipantId,
}

impl HeaderExt {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ParticipantId => "pb-participant-id",
        }
    }
}

/// Configuration shared across handlers
#[derive(Clone)]
pub struct ApiConfig {
    pub base_path: String,    // e.g. "/api/v1"
    pub default_host: String, // fallback if no Host header, e.g. "localhost:3000"
}

/// Error type for api operations
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("join failed: {0}")]
    JoinError(#[from] controller::ControllerError),
    #[error("server is busy, please try again later")]
    ServiceUnavailable,
    #[error("failed to construct response URL")]
    BadUrl,
    #[error("{0}")]
    Unknown(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            ApiError::JoinError(controller::ControllerError::OfferInvalid(_))
            | ApiError::JoinError(controller::ControllerError::OfferRejected(_)) => {
                StatusCode::BAD_REQUEST
            }
            ApiError::JoinError(controller::ControllerError::ServiceUnavailable)
            | ApiError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::JoinError(controller::ControllerError::Unknown(_))
            | ApiError::JoinError(controller::ControllerError::IOError(_))
            | ApiError::BadUrl
            | ApiError::Unknown(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (status, self.to_string()).into_response()
    }
}

/// Build an absolute URL for Location header
fn build_location(headers: &HeaderMap, cfg: &ApiConfig, path: &str) -> Result<String, ApiError> {
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

    url.parse::<Uri>().map_err(|_| ApiError::BadUrl)?;

    Ok(url)
}

/// Create a new participant in a room
///
/// Creates a new participant by processing a WebRTC offer and returning an answer.
/// The participant ID is generated and returned in both the Location header and
/// the pb-participant-id header.
#[utoipa::path(
    post,
    path = "/rooms/{external_room_id}/participants",
    request_body(content = String, description = "WebRTC SDP offer", content_type = "application/sdp"),
    params(
        ("external_room_id" = String, Path, description = "External room identifier")
    ),
    responses(
        (status = 201, description = "Participant created successfully", body = String,
            headers(
                ("Location" = String, description = "URL of the created participant resource"),
                ("pb-participant-id" = String, description = "Internal participant ID")
            ),
            content_type = "application/sdp"
        ),
        (status = 400, description = "Invalid or rejected offer", body = String, content_type = "text/plain"),
        (status = 500, description = "Internal server error", body = String, content_type = "text/plain"),
        (status = 503, description = "Service unavailable", body = String, content_type = "text/plain")
    ),
    tag = "participants"
)]
#[axum::debug_handler]
async fn create_participant(
    Path(room_id): Path<ExternalRoomId>,
    State((mut con, cfg)): State<(controller::ControllerHandle, ApiConfig)>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    headers: HeaderMap,
    raw_offer: String,
) -> Result<impl IntoResponse, ApiError> {
    let room_id = Arc::new(RoomId::new(room_id));
    let participant_id = Arc::new(ParticipantId::new());

    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    con.send(controller::ControllerMessage::Allocate(
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
    response_headers.insert(
        HeaderExt::ParticipantId.as_str(),
        participant_id.internal.parse().unwrap(),
    );

    Ok((StatusCode::CREATED, response_headers, answer_sdp))
}

/// Delete a participant from a room
///
/// Removes a participant from the specified room. This will clean up all
/// resources associated with the participant.
#[utoipa::path(
    delete,
    path = "/rooms/{external_room_id}/participants/{participant_id}",
    params(
        ("external_room_id" = String, Path, description = "External room identifier"),
        ("participant_id" = String, Path, description = "Participant identifier")
    ),
    responses(
        (status = 204, description = "Participant deleted successfully"),
        (status = 500, description = "Internal server error", body = String, content_type = "text/plain")
    ),
    tag = "participants"
)]
#[axum::debug_handler]
async fn delete_participant(
    Path((room_id, participant_id)): Path<(ExternalRoomId, ParticipantId)>,
    State((mut con, _cfg)): State<(controller::ControllerHandle, ApiConfig)>,
) -> Result<impl IntoResponse, ApiError> {
    let room_id = Arc::new(RoomId::new(room_id));
    let participant_id = Arc::new(participant_id);

    let _ = con
        .send(controller::ControllerMessage::RemoveParticipant(
            room_id,
            participant_id,
        ))
        .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Build OpenAPI spec with dynamic server configuration
fn build_openapi(base_path: &str) -> utoipa::openapi::OpenApi {
    use utoipa::openapi::{ContactBuilder, InfoBuilder, OpenApi as OpenApiSpec, ServerBuilder};

    let info = InfoBuilder::new()
        .title("PulseBeam API")
        .version("1.0.0")
        .description(Some("API for managing PulseBeam room & participants"))
        .contact(Some(
            ContactBuilder::new()
                .name(Some("API Support"))
                .email(Some("lukas@pulsebeam.dev"))
                .build(),
        ))
        .build();

    let mut openapi = OpenApiSpec::new(info, utoipa::openapi::path::Paths::new());

    // Add server with the configured base path
    openapi.servers = Some(vec![
        ServerBuilder::new()
            .url(base_path)
            .description(Some("API Server"))
            .build(),
    ]);

    // Merge in the generated paths
    let generated = ApiDoc::openapi();
    openapi.paths = generated.paths;
    openapi.components = generated.components;
    openapi.tags = generated.tags;

    openapi
}

/// OpenAPI documentation structure
#[derive(OpenApi)]
#[openapi(
    paths(
        create_participant,
        delete_participant,
    ),
    tags(
        (name = "participants", description = "Participant management endpoints"),
    )
)]
struct ApiDoc;

/// Router setup with OpenAPI documentation
pub fn router(controller: controller::ControllerHandle, cfg: ApiConfig) -> Router {
    let openapi = build_openapi(&cfg.base_path);

    let api = Router::new()
        .route(
            "/rooms/{external_room_id}/participants",
            post(create_participant),
        )
        .route(
            "/rooms/{external_room_id}/participants/{participant_id}",
            delete(delete_participant),
        );

    Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi))
        .nest(&cfg.base_path, api)
        .with_state((controller, cfg))
}
