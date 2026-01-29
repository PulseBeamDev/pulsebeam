use axum::{
    Router,
    extract::{Path, State},
    http::{
        HeaderMap, StatusCode, Uri,
        header::{HeaderName, HeaderValue},
    },
    response::IntoResponse,
    routing::{patch, post},
};
use axum_extra::{TypedHeader, headers::ContentType};
use hyper::header::LOCATION;
use pulsebeam_runtime::mailbox::TrySendError;
use serde::Serialize;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::controller;
use crate::entity::{ParticipantId, RoomId};

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

/// Request headers for participant reconnection
#[derive(Debug, ToSchema)]
pub struct ReconnectionRequestHeaders {
    /// Video track ID
    pub video_track_id: String,
    /// Audio track ID
    pub audio_track_id: String,
    /// Ed25519 signature proving ownership
    pub signature: String,
    /// ETag for concurrency control
    /// https://www.rfc-editor.org/rfc/rfc9110.html#name-etag
    pub etag: String,
}

impl ReconnectionRequestHeaders {
    pub fn from_header_map(headers: &HeaderMap) -> Result<Self, ApiError> {
        let video_track_id = headers
            .get("pb-vid")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::BadRequest("missing pb-vid header".to_string()))?
            .to_string();

        let audio_track_id = headers
            .get("pb-aid")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::BadRequest("missing pb-aid header".to_string()))?
            .to_string();

        let signature = headers
            .get("pb-sig")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::BadRequest("missing pb-sig header".to_string()))?
            .to_string();

        let etag = headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::BadRequest("missing ETag header".to_string()))?
            .to_string();

        Ok(Self {
            video_track_id,
            audio_track_id,
            signature,
            etag,
        })
    }
}

/// Response headers for participant creation
#[derive(Debug, Serialize, ToSchema)]
pub struct ParticipantResponseHeaders {
    /// URL of the created participant resource
    #[serde(rename = "Location")]
    pub location: String,
    /// Internal participant ID
    #[serde(rename = "pb-participant-id")]
    pub participant_id: String,
}

impl ParticipantResponseHeaders {
    pub fn to_header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, self.location.parse().unwrap());
        headers.insert(
            HeaderName::from_static(HeaderExt::ParticipantId.as_str()),
            HeaderValue::from_str(&self.participant_id).unwrap(),
        );
        headers
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
    #[error("too many requests, please try again later.")]
    RateLimited,
    #[error("server is busy, please try again later")]
    ServiceUnavailable,
    #[error("failed to construct response URL")]
    BadUrl,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("{0}")]
    Unknown(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            ApiError::JoinError(controller::ControllerError::OfferInvalid(_))
            | ApiError::JoinError(controller::ControllerError::OfferRejected(_))
            | ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::JoinError(controller::ControllerError::ServiceUnavailable)
            | ApiError::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
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
                ("pb-participant-id" = String, description = "participant ID")
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
    Path(room_id): Path<RoomId>,
    State((mut con, cfg)): State<(controller::ControllerHandle, ApiConfig)>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    headers: HeaderMap,
    raw_offer: String,
) -> Result<impl IntoResponse, ApiError> {
    let participant_id = ParticipantId::new();

    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    con.try_send(controller::CreateParticipant {
        room_id: room_id.clone(),
        participant_id: participant_id.clone(),
        offer: raw_offer,
        reply_tx: answer_tx,
    })
    .map_err(|e| match e {
        TrySendError::Full(_) => ApiError::RateLimited,
        TrySendError::Closed(_) => ApiError::ServiceUnavailable,
    })?;

    let answer_sdp = answer_rx
        .await
        .map_err(|_| controller::ControllerError::ServiceUnavailable)??;

    let path = format!(
        "/rooms/{}/participants/{}",
        &room_id.external(),
        &participant_id
    );
    let location_url = build_location(&headers, &cfg, &path)?;

    let response_headers = ParticipantResponseHeaders {
        location: location_url,
        participant_id: participant_id.to_string(),
    };

    Ok((
        StatusCode::CREATED,
        response_headers.to_header_map(),
        answer_sdp,
    ))
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
    Path((room_id, participant_id)): Path<(RoomId, ParticipantId)>,
    State((mut con, _cfg)): State<(controller::ControllerHandle, ApiConfig)>,
) -> Result<impl IntoResponse, ApiError> {
    con.try_send(controller::DeleteParticipant {
        room_id,
        participant_id,
    })
    .map_err(|e| match e {
        TrySendError::Full(_) => ApiError::RateLimited,
        TrySendError::Closed(_) => ApiError::ServiceUnavailable,
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Reconnect a participant to a room
///
/// Allows a participant to reconnect to a room with new WebRTC offer,
/// using their existing track IDs and providing authentication signature.
#[utoipa::path(
    patch,
    path = "/rooms/{external_room_id}/participants/{participant_id}",
    request_body(content = String, description = "WebRTC SDP offer for reconnection", content_type = "application/sdp"),
    params(
        ("external_room_id" = String, Path, description = "External room identifier"),
        ("participant_id" = String, Path, description = "Participant identifier")
    ),
    responses(
        (status = 200, description = "Reconnection successful", body = String,
            content_type = "application/sdp"
        ),
        (status = 400, description = "Invalid request or missing headers", body = String, content_type = "text/plain"),
        (status = 401, description = "Invalid signature", body = String, content_type = "text/plain"),
        (status = 412, description = "Precondition failed - ETag mismatch", body = String, content_type = "text/plain"),
        (status = 500, description = "Internal server error", body = String, content_type = "text/plain"),
        (status = 503, description = "Service unavailable", body = String, content_type = "text/plain")
    ),
    tag = "participants"
)]
#[axum::debug_handler]
async fn patch_participant(
    Path((room_id, participant_id)): Path<(RoomId, ParticipantId)>,
    State((mut con, _cfg)): State<(controller::ControllerHandle, ApiConfig)>,
    TypedHeader(_content_type): TypedHeader<ContentType>,
    headers: HeaderMap,
    _raw_offer: String,
) -> Result<impl IntoResponse, ApiError> {
    let h = ReconnectionRequestHeaders::from_header_map(&headers)?;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    let msg = controller::PatchParticipant {
        room_id,
        participant_id,
        video_track_id: h.video_track_id,
        audio_track_id: h.audio_track_id,
        signature: h.signature,
        etag: h.etag,
        reply_tx,
    };
    con.try_send(msg).map_err(|e| match e {
        TrySendError::Full(_) => ApiError::RateLimited,
        TrySendError::Closed(_) => ApiError::ServiceUnavailable,
    })?;

    let answer_sdp = reply_rx
        .await
        .map_err(|_| controller::ControllerError::ServiceUnavailable)??;

    Ok((StatusCode::OK, answer_sdp))
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

/// OpenAPI documentation structure (without servers, we'll set those dynamically)
#[derive(OpenApi)]
#[openapi(
    paths(
        create_participant,
        patch_participant,
        delete_participant,
    ),
    components(
        schemas(ParticipantResponseHeaders, ReconnectionRequestHeaders)
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
            patch(patch_participant).delete(delete_participant),
        );

    Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi))
        .nest(&cfg.base_path, api)
        .with_state((controller, cfg))
}
